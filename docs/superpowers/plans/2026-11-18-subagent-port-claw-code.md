# Subagent Lifecycle, Control and Display Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give mehh subagents a machine-readable on-disk lifecycle, a global concurrency budget, per-child steer/kill handles, and a nested TUI view driven by structured events.

**Architecture:** A new `subagent_lifecycle` module owns the manifest data model and disk writer. `SubagentTool` builds a manifest per child, emits structured updates in `AgentToolResult.details`, and finalizes it on every exit path. A shared `SubagentControl` registry holds one handle per child (child cancel token + steering queue), providing the global semaphore and the list/steer/kill APIs. The interactive renderer branches on `details.subagent` to show a nested group instead of a generic tool card.

**Tech Stack:** Rust, `tokio`, `tokio-util`, `uuid`, `serde_json`, existing `mehh-agent`/`mehh-coding-agent` types.

**Spec:** `docs/superpowers/specs/2026-11-18-subagent-port-claw-code-design.md`

## Global Constraints

- Verify with `cargo test -p mehh-coding-agent` from the workspace root.
- No new dependencies. `uuid`, `serde`, `serde_json`, `tokio`, `tokio-util`, `async-trait` are already in `crates/mehh-coding-agent/Cargo.toml`.
- Timestamps are `i64` epoch milliseconds from `mehh_ai::now_ms()` (returns `i64`, same as `UserMessage.timestamp`); field names end in `_ms`. Never use a function named `iso8601_now`.
- Manifest directory: `<cwd>/.mehh-agents` unless env `MEHH_AGENT_STORE` overrides it. Tests must derive the expected path with `subagent_lifecycle::agent_store_dir(&cwd)` rather than hardcoding it, so an ambient env var cannot break them. Do not add the directory to `.gitignore` in this change.
- Reuse the existing `SUBAGENT_TIMEOUT_SECS` constant at `subagent.rs:27` as the default. Do not introduce a second timeout constant. The new `timeout_secs` parameter overrides the `Duration::from_secs(SUBAGENT_TIMEOUT_SECS)` currently hard-coded at `subagent.rs:228`.
- Integration tests calling `sub.execute(...)` must `use mehh_agent::AgentTool;` — `execute` is a trait method, not inherent.
- Preserve the existing three modes and their tool filters. Do not add recursive subagent spawning.
- Structured child events live in `AgentToolResult.details` under key `"subagent"`. Keep text updates intact. Emit a `Started` update at child start so the TUI and tests can learn the child id before any tool runs.
- `SubagentControl` is optional on `SubagentTool`; direct `SubagentTool::new(...)` construction must keep compiling and working. All `Self { ... }` child literals (`subagent.rs:311-320`, `:365-374`) must include `control: self.control.clone()`.
- `kill` cancels exactly one child's token, and that token must be an **ancestor** of the child's `config.cancel`. It must not call `AgentSession::abort`.
- Finalization is the single source of terminal events: push the terminal `SubagentEvent` into the manifest **and** emit it through `on_update` before writing the manifest.
- `run_agent_loop` checks cancellation only at turn boundaries (`mehh-agent/src/agent_loop.rs:113,151,335,363`); while awaiting `stream.next()` (`:440`) it does not. Therefore `run_child_prompt` must wrap the timeout future in a `tokio::select!` with a `cancel.cancelled()` branch returning `Err("Subagent execution was aborted")`. This stays inside `mehh-coding-agent` (respecting the no-`mehh-agent`-changes non-goal) and makes `kill`/abort effective even against providers whose streams do not self-interrupt.
- Commits: `feat(subagent): <ascii subject>`, explicit paths only.

## Review Focus

1. **Timeout or abort leaves status `running` forever** — terminal manifest must include a terminal event and non-null `completed_at_ms`. Test in Task 2 (timeout + abort).
2. **Global budget is still per-call** — two overlapping `subagent` tool calls must contend on one semaphore, not allocate two `Semaphore(4)` pools. Test in Task 3 with two concurrent real `execute` calls plus a blocked third acquire.
3. **`kill` takes down the parent turn or does nothing** — cancelling a child must stop that child's loop and leave the parent token intact. Test in Task 3 with a real hanging child.
4. **Steer targets the wrong child / unknown id** — unknown id returns `false`; two children do not share a queue; a steered message is actually consumed by the child loop. Tests in Task 3 and Task 4.
5. **Nested TUI rows get cross-wired between parallel tool calls** — the group is keyed by the parent `tool_call_id`; rows are keyed by `child_id`. Test in Task 5.

---

### Task 1: Subagent lifecycle data model and manifest writer

**Files:**
- Create: `crates/mehh-coding-agent/src/subagent_lifecycle.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs:20` (add `pub mod subagent_lifecycle;`) and `:45` (add `pub use subagent_lifecycle::*;`)
- Test: `crates/mehh-coding-agent/src/subagent_lifecycle.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces (all `pub`, in `subagent_lifecycle`):
  - `enum SubagentStatus { Running, Completed, Failed }` — `serde(rename_all = "snake_case")`
  - `enum SubagentEventKind { Started, ToolStarted, ToolFinished, Blocked, Finished, Aborted, TimedOut }` — snake_case
  - `struct SubagentEvent { pub kind: SubagentEventKind, pub at_ms: i64, pub detail: Option<String> }`
  - `enum SubagentFailureClass { Transport, ToolRuntime, Compile, Test, Mcp, Timeout, Aborted, Infra }` + `fn classify_failure(error: &str) -> SubagentFailureClass`
  - `fn derive_state(status: &SubagentStatus, failure_class: Option<&SubagentFailureClass>) -> &'static str`
  - `struct SubagentManifest { pub agent_id: String, pub mode: String, pub model_id: String, pub status: SubagentStatus, pub created_at_ms: i64, pub completed_at_ms: Option<i64>, pub events: Vec<SubagentEvent>, pub failure_class: Option<SubagentFailureClass>, pub derived_state: String, pub result: Option<String>, pub error: Option<String> }`
  - `fn new_agent_id() -> String` — `format!("sub-{}", uuid::Uuid::now_v7())`
  - `fn new_manifest(agent_id: String, mode: String, model_id: String, now_ms: i64) -> SubagentManifest` — sets `Running`, `derived_state = "working"`, one `Started` event
  - `fn agent_store_dir(cwd: &std::path::Path) -> std::path::PathBuf`
  - `fn write_manifest(dir: &std::path::Path, manifest: &SubagentManifest) -> Result<std::path::PathBuf, String>` — creates dir, writes pretty JSON to `<dir>/<agent_id>.json`, returns path

- [ ] **Step 1: Write failing tests in `subagent_lifecycle.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_manifest_starts_running_with_started_event() {
        let m = new_manifest("sub-1".into(), "research".into(), "m".into(), 1000);
        assert_eq!(m.status, SubagentStatus::Running);
        assert_eq!(m.derived_state, "working");
        assert_eq!(m.events.len(), 1);
        assert_eq!(m.events[0].kind, SubagentEventKind::Started);
        assert_eq!(m.events[0].at_ms, 1000);
        assert_eq!(m.completed_at_ms, None);
    }

    #[test]
    fn classify_failure_maps_known_signals_and_defaults_to_infra() {
        assert_eq!(classify_failure("tool runtime exploded"), SubagentFailureClass::ToolRuntime);
        assert_eq!(classify_failure("compile failed"), SubagentFailureClass::Compile);
        assert_eq!(classify_failure("broken pipe"), SubagentFailureClass::Transport);
        assert_eq!(classify_failure("Subagent timed out after 180s"), SubagentFailureClass::Timeout);
        assert_eq!(classify_failure("no idea"), SubagentFailureClass::Infra);
    }

    #[test]
    fn write_manifest_persists_terminal_state_with_epoch_ms() {
        let dir = std::env::temp_dir().join(format!("mehh_lc_test_{}", uuid::Uuid::now_v7()));
        let mut m = new_manifest("sub-2".into(), "coding".into(), "m".into(), 1000);
        m.status = SubagentStatus::Failed;
        m.completed_at_ms = Some(2500);
        m.failure_class = Some(SubagentFailureClass::Test);
        m.derived_state = derive_state(&m.status, m.failure_class.as_ref()).to_string();
        m.error = Some("test failed".into());
        let path = write_manifest(&dir, &m).unwrap();
        assert_eq!(path.file_name().unwrap(), "sub-2.json");
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(raw["status"], "failed");
        assert_eq!(raw["created_at_ms"], 1000);
        assert_eq!(raw["completed_at_ms"], 2500);
        assert_eq!(raw["failure_class"], "test");
        assert!(raw.get("iso8601_now").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent subagent_lifecycle::tests -- --nocapture`
Expected: compile failure, module does not exist yet.

- [ ] **Step 3: Implement `subagent_lifecycle.rs`**

Define the types and functions listed in Interfaces. `classify_failure` checks lowercased substrings in this order: `timed out` → `Timeout`, `aborted` → `Aborted`, `compile`/`build failed` → `Compile`, `test` → `Test`, `tool runtime`/`tool failed` → `ToolRuntime`, `mcp` → `Mcp`, `transport`/`pipe`/`connection` → `Transport`, else `Infra`. `derive_state` returns `working` for `Running`, `finished_cleanable` for `Completed`, and the failure-class name for `Failed`. `agent_store_dir` reads `MEHH_AGENT_STORE`, else `cwd.join(".mehh-agents")`. Add the two `lib.rs` lines.

- [ ] **Step 4: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent subagent_lifecycle::tests`
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/mehh-coding-agent/src/subagent_lifecycle.rs crates/mehh-coding-agent/src/lib.rs
git commit -m "feat(subagent): add lifecycle manifest data model and writer"
```

---

### Task 2: Structured child events and terminal persistence

**Files:**
- Modify: `crates/mehh-coding-agent/src/subagent.rs:75-106` (schema), `:128-250` (`run_child_prompt`), `:267-381` (`execute`), `:384-419` (in-file tests)
- Create: `crates/mehh-coding-agent/tests/test_subagent_lifecycle.rs`
- Test: both files above

**Interfaces:**
- Consumes: all Task 1 types; `AgentToolResult` from `mehh_agent::types` (pub `details: Option<Value>`, derives `Default`); `SUBAGENT_TIMEOUT_SECS` at `subagent.rs:27`.
- Produces:
  - `fn subagent_update(child_id: &str, mode: &str, event: &SubagentEvent) -> AgentToolResult` — `details = Some(json!({"subagent": {"child_id":..., "mode":..., "event_kind":..., "detail":...}}))`, text kept as `"[subagent:{mode}] ..."`
  - `run_child_prompt` gains parameters `manifest: &mut SubagentManifest`, `timeout_secs: u64`, and `steering_queue: Option<Arc<Mutex<VecDeque<Message>>>>`; it emits a `Started` update at child start, pushes `ToolStarted`/`ToolFinished` events on tool start/end, and finalizes exactly once
  - `parameters_schema()` gains optional `timeout_secs` integer, `minimum: 1`, `maximum: 180`
  - `execute()` creates the manifest, passes it into both the single-task and batch paths, finalizes any child whose `JoinHandle` returns `Err(JoinError)`, and returns an `AgentToolResult` with `details` set

- [ ] **Step 1: Write failing tests in `tests/test_subagent_lifecycle.rs`**

```rust
use std::sync::Arc;
use mehh_agent::AgentTool; // required: execute() is a trait method
use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_lifecycle::agent_store_dir;

struct HangingProvider;

#[async_trait::async_trait]
impl mehh_ai::LlmProvider for HangingProvider {
    fn stream(
        &self,
        _model: &mehh_ai::Model,
        _context: &mehh_ai::Context,
        _options: &mehh_ai::StreamOptions,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> mehh_ai::AssistantEventStream {
        let (tx, rx) = mehh_ai::create_event_stream();
        std::mem::forget(tx); // channel stays open forever, so the loop hangs until timeout
        rx
    }
}

fn manifest_files(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(agent_store_dir(cwd)).unwrap()
        .map(|e| e.unwrap().path()).collect()
}

#[tokio::test]
async fn successful_child_writes_completed_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ok_{}", uuid::Uuid::now_v7()));
    let faux = Arc::new(FauxProvider::new());
    faux.add_response(faux_assistant_message("done"));
    let sub = SubagentTool::new(dir.clone(), faux_model(), faux, None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    let res = sub.execute("tc1", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap();
    assert!(res.details.is_some(), "result must carry structured subagent details");

    let files = manifest_files(&dir);
    assert_eq!(files.len(), 1);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "completed");
    assert!(m["completed_at_ms"].is_number());
    assert_eq!(m["events"][0]["kind"], "started");
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "finished"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn timeout_writes_terminal_timed_out_manifest() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_to_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(1));
    let err = sub.execute("tc2", params, tokio_util::sync::CancellationToken::new(), None).await.unwrap_err();
    assert!(err.contains("timed out"));

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "timeout");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "timed_out"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn abort_writes_aborted_manifest_with_completed_at_ms() {
    let dir = std::env::temp_dir().join(format!("mehh_sub_ab_{}", uuid::Uuid::now_v7()));
    let sub = SubagentTool::new(dir.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0);
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let cancel = tokio_util::sync::CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move { sub.execute("tc3", params, task_cancel, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let err = tokio::time::timeout(std::time::Duration::from_secs(2), task).await.unwrap().unwrap().unwrap_err();
    assert!(err.contains("aborted") || err.contains("Aborted"), "unexpected: {err}");

    let files = manifest_files(&dir);
    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(m["status"], "failed");
    assert_eq!(m["failure_class"], "aborted");
    assert!(m["completed_at_ms"].is_number());
    assert!(m["events"].as_array().unwrap().iter().any(|e| e["kind"] == "aborted"));
    std::fs::remove_dir_all(dir).unwrap();
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent --test test_subagent_lifecycle`
Expected: compile failure (`timeout_secs` absent from schema; no `Started`/terminal events in manifest).

- [ ] **Step 3: Implement in `subagent.rs`**

Parse `timeout_secs` (default `SUBAGENT_TIMEOUT_SECS`) in `execute()` and pass it to `run_child_prompt`, replacing the hard-coded `Duration::from_secs(SUBAGENT_TIMEOUT_SECS)` at `:228` with `Duration::from_secs(timeout_secs)`. Wrap that timeout future in a `tokio::select!` with a second branch on `cancel.cancelled()` that returns `Err("Subagent execution was aborted".to_string())`; keep the `tokio::time::timeout` error text containing "timed out" for the timeout branch. Emit a `Started` `subagent_update` through `on_update` immediately after creating the manifest so the child id is observable before any tool runs. Restructure `run_child_prompt` so its body computes a `Result<String, String>` outcome in an inner block, then runs exactly one finalization step. Finalization must: set `Completed` + `result` on `Ok`, or `Failed` + `error` on `Err`; classify failure (if `cancel.is_cancelled()` on the timeout/abort path use `Aborted`, else `classify_failure(&error)`); derive state; set `completed_at_ms = Some(now_ms())`; **push a terminal `SubagentEvent` (`Finished`/`Aborted`/`TimedOut`) into `manifest.events` and call `subagent_update` for it**; then call `write_manifest`. In `execute()`, write the manifest after the single/batch path returns and set `details` on the returned `AgentToolResult` to `Some(json!({"subagent": {"child_id": manifest.agent_id, "mode": mode, "status": manifest.status, "manifest": path}}))` using `..Default::default()` for other fields. For batch mode: each task gets its own manifest; when a `JoinHandle` returns `Err(join_err)`, the parent finalizes that child with `Failed`, error `format!("subagent task panicked: {join_err}")`, and a terminal `Aborted` event, so no child stays `running`.

- [ ] **Step 4: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent --test test_subagent_lifecycle`
Expected: 3 tests pass.

- [ ] **Step 5: Full crate regression**

Run: `cargo test -p mehh-coding-agent`
Expected: all tests pass, including `test_subagent_multimode` and `test_subagent_governance`.

- [ ] **Step 6: Commit**

```bash
git add crates/mehh-coding-agent/src/subagent.rs crates/mehh-coding-agent/tests/test_subagent_lifecycle.rs
git commit -m "feat(subagent): emit structured lifecycle events and persist terminal state"
```

---

### Task 3: Shared child registry, global budget, and per-child control

**Files:**
- Create: `crates/mehh-coding-agent/src/subagent_control.rs`
- Modify: `crates/mehh-coding-agent/src/lib.rs` (module + re-export)
- Modify: `crates/mehh-coding-agent/src/subagent.rs` (new optional field + builder + registration in both paths + `control` in child literals)
- Modify: `crates/mehh-coding-agent/src/agent_session.rs` (field init in `new_with_trust` only; `refresh_subagent_tool`; accessor)
- Create: `crates/mehh-coding-agent/tests/test_subagent_control.rs`

**Interfaces:**
- Produces in `subagent_control`:
  - `pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 4;`
  - `pub struct SubagentHandle { pub agent_id: String, pub mode: String, pub started_ms: i64, pub cancel: CancellationToken, pub steering_queue: Arc<Mutex<VecDeque<Message>>> }`
  - `pub struct SubagentSummary { pub agent_id: String, pub mode: String, pub elapsed_ms: i64 }`
  - `pub struct SubagentControl { permits: Arc<Semaphore>, children: Arc<Mutex<HashMap<String, SubagentHandle>>> }`
  - `pub fn new(max_concurrent: usize) -> Self`
  - `pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit` — implemented as `Arc::clone(&self.permits).acquire_owned().await.expect("semaphore closed")`
  - `pub fn register(&self, agent_id: String, mode: String, cancel: CancellationToken) -> SubagentHandle`
  - `pub fn unregister(&self, agent_id: &str)`
  - `pub fn list(&self) -> Vec<SubagentSummary>`
  - `pub fn steer(&self, agent_id: &str, text: &str) -> bool` — pushes a `Message::User` into that child's queue; `false` for unknown id
  - `pub fn kill(&self, agent_id: &str) -> bool` — cancels only that child's token and removes the handle; `false` for unknown id
- `SubagentTool` gains `pub control: Option<Arc<SubagentControl>>` and `pub fn with_control(mut self, control: Arc<SubagentControl>) -> Self`.
- `AgentSession` gains `pub subagent_control: Arc<SubagentControl>` and `pub fn subagent_control(&self) -> Arc<SubagentControl>`.

- [ ] **Step 1: Write failing tests in `tests/test_subagent_control.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;
use mehh_agent::AgentTool;
use mehh_ai::{faux_model, Context, Model, StreamOptions};
use mehh_coding_agent::subagent::SubagentTool;
use mehh_coding_agent::subagent_control::{SubagentControl, DEFAULT_MAX_CONCURRENT_SUBAGENTS};

struct HangingProvider;
#[async_trait::async_trait]
impl mehh_ai::LlmProvider for HangingProvider {
    fn stream(&self, _m: &Model, _c: &Context, _o: &StreamOptions, _x: tokio_util::sync::CancellationToken) -> mehh_ai::AssistantEventStream {
        let (tx, rx) = mehh_ai::create_event_stream();
        std::mem::forget(tx);
        rx
    }
}

#[tokio::test]
async fn global_budget_is_shared_and_limited() {
    let control = Arc::new(SubagentControl::new(2));
    let _a = control.acquire().await;
    let _b = control.acquire().await;
    let blocked = tokio::time::timeout(Duration::from_millis(50), control.clone().acquire()).await;
    assert!(blocked.is_err(), "third acquire must block while both permits are held");
    assert_eq!(DEFAULT_MAX_CONCURRENT_SUBAGENTS, 4);
}

#[tokio::test]
async fn two_overlapping_subagent_calls_share_one_budget() {
    let control = Arc::new(SubagentControl::new(2));
    let cwd = std::env::temp_dir().join(format!("mehh_budget_{}", uuid::Uuid::now_v7()));
    let mk = || SubagentTool::new(cwd.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0)
        .with_control(control.clone());
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let c1 = tokio_util::sync::CancellationToken::new();
    let c2 = tokio_util::sync::CancellationToken::new();
    let c3 = tokio_util::sync::CancellationToken::new();
    let t1 = tokio::spawn(mk().execute("a", params.clone(), c1.clone(), None));
    let t2 = tokio::spawn(mk().execute("b", params.clone(), c2.clone(), None));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(control.list().len(), 2, "two children from two separate calls");
    let t3 = tokio::spawn(mk().execute("c", params, c3.clone(), None));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tokio::time::timeout(Duration::from_millis(50), control.clone().acquire()).await.is_err(),
        "third child must be blocked by the shared budget");
    c1.cancel(); c2.cancel(); c3.cancel();
    for t in [t1, t2, t3] {
        let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
    }
    std::fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn kill_targets_one_child_and_steer_reaches_only_that_child() {
    let control = Arc::new(SubagentControl::new(4));
    let parent = tokio_util::sync::CancellationToken::new();
    let a = control.register("sub-a".into(), "coding".into(), parent.child_token());
    let b = control.register("sub-b".into(), "research".into(), parent.child_token());

    assert!(control.steer("sub-a", "new instruction"));
    assert_eq!(a.steering_queue.lock().unwrap().len(), 1);
    assert_eq!(b.steering_queue.lock().unwrap().len(), 0);
    assert!(!control.steer("missing", "x"));

    assert!(control.kill("sub-a"));
    assert!(a.cancel.is_cancelled());
    assert!(!b.cancel.is_cancelled());
    assert!(!parent.is_cancelled(), "parent turn must survive child kill");
    assert!(!control.kill("sub-a"), "already-unregistered id returns false");
    control.unregister("sub-a");
    assert_eq!(control.list().len(), 1);
}

#[tokio::test]
async fn kill_stops_a_real_child_loop() {
    let control = Arc::new(SubagentControl::new(4));
    let cwd = std::env::temp_dir().join(format!("mehh_kill_{}", uuid::Uuid::now_v7()));
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let child_id = Arc::new(std::sync::Mutex::new(None::<String>));
    let seen = child_id.clone();
    let on_update: mehh_agent::AgentToolUpdateCallback = Arc::new(move |res| {
        if let Some(id) = res.details.as_ref().and_then(|d| d.get("subagent")).and_then(|s| s.get("child_id")).and_then(|v| v.as_str()) {
            if seen.lock().unwrap().is_none() {
                *seen.lock().unwrap() = Some(id.to_string());
                let _ = tx.send(id.to_string());
            }
        }
    });
    let sub = SubagentTool::new(cwd.clone(), faux_model(), Arc::new(HangingProvider), None, vec![], 0)
        .with_control(control.clone());
    let mut params = serde_json::Map::new();
    params.insert("prompt".into(), serde_json::json!("hi"));
    params.insert("timeout_secs".into(), serde_json::json!(30));
    let token = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(async move { sub.execute("tc", params, token, Some(on_update)).await });
    let id = tokio::time::timeout(Duration::from_secs(2), rx).await.unwrap().unwrap();
    assert!(control.kill(&id), "registered child must be killable");
    let result = tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert!(result.is_err(), "killed child must terminate with error");
    std::fs::remove_dir_all(cwd).unwrap();
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent --test test_subagent_control`
Expected: compile failure, `subagent_control` module missing.

- [ ] **Step 3: Implement `subagent_control.rs`**

`register` inserts a handle and returns a clone. `steer` pushes `Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: now_ms() })`. `kill` looks up the id, calls `cancel.cancel()`, removes the handle. `unregister` removes by id. `list` maps remaining handles to `SubagentSummary` with `elapsed_ms = now_ms() - started_ms`. `acquire` uses `acquire_owned().await.expect(...)`.

- [ ] **Step 4: Wire into `SubagentTool`**

Add the `control` field and `with_control` builder. In single-task and batch paths: obtain a permit from `self.control` when present (else a local `Semaphore::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS)`), create the child id, call `control.register(id, mode, parent_cancel.clone())`, and **pass `handle.cancel.clone()` as the `cancel` argument to `run_child_prompt`** so `config.cancel = handle.cancel.child_token()` — this makes `kill` effective. Set `config.steering_queue = handle.steering_queue.clone()` and `config.steering_mode = QueueMode::OneAtATime` inside `run_child_prompt`. Call `unregister` on every exit path, including the `JoinError` branch from Task 2. Add `control: self.control.clone()` to both child struct literals at `subagent.rs:311-320` and `:365-374`.

- [ ] **Step 5: Wire into `AgentSession`**

In `new_with_trust` only (`new` delegates to it at `agent_session.rs:64-72`), construct `SubagentControl::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS)`, store it in the new field, and pass `.with_control(control.clone())` at both subagent construction sites (`:145-156` and `refresh_subagent_tool` at `:530`). Add the accessor.

- [ ] **Step 6: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent --test test_subagent_control`
Expected: 4 tests pass.

- [ ] **Step 7: Steer consumed by a child loop**

Add a test with a recording provider: its `stream()` pushes an `AssistantMessageEvent::Done` carrying a message whose text is the first `Context` message text, into the `AssistantEventStream`. Start a child with a steered message queued through `control.steer(&id, "ROUTE_ME")`, then assert the provider observed `"ROUTE_ME"` in the context. This proves `config.steering_queue` is wired, not just that the queue holds a message.

- [ ] **Step 8: Full crate regression**

Run: `cargo test -p mehh-coding-agent`
Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/mehh-coding-agent/src/subagent_control.rs crates/mehh-coding-agent/src/lib.rs crates/mehh-coding-agent/src/subagent.rs crates/mehh-coding-agent/src/agent_session.rs crates/mehh-coding-agent/tests/test_subagent_control.rs
git commit -m "feat(subagent): add shared child registry, budget and steer/kill controls"
```

---

### Task 4: RPC commands for listing and controlling children

**Files:**
- Modify: `crates/mehh-coding-agent/src/rpc.rs:459-479` (add three match arms)
- Create: `crates/mehh-coding-agent/tests/test_subagent_rpc.rs`

**Interfaces:**
- Consumes: `AgentSession::subagent_control()` from Task 3; `run_rpc_mode<R, W>(&mut AgentSession, R, W) -> anyhow::Result<()>` at `rpc.rs:130`, with commands selected by the JSON `type` field (`rpc.rs:171`).
- Produces:
  - `"subagent_list"` → `send_success(id, "subagent_list", Some(json!({"agents": [...] })))`
  - `"subagent_steer"` with `"agent_id"` + `"message"` → `send_success` with `{"ok": bool}`
  - `"subagent_kill"` with `"agent_id"` → `send_success` with `{"ok": bool}`

- [ ] **Step 1: Write failing test in `tests/test_subagent_rpc.rs`**

```rust
use std::sync::Arc;
use tokio::io::{AsyncReadExt, BufReader};
use mehh_coding_agent::agent_session::AgentSession;
use mehh_coding_agent::rpc::run_rpc_mode;
use mehh_coding_agent::session::SessionManager;
use mehh_coding_agent::settings::Settings;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn rpc_can_list_steer_and_kill_a_child() {
    let dir = std::env::temp_dir().join(format!("mehh_rpc_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut session = AgentSession::new(
        dir.clone(), mehh_ai::faux_model(), Arc::new(mehh_ai::FauxProvider::new()),
        SessionManager::in_memory(&dir), Settings::default(),
    );
    let child = session.subagent_control().register("sub-x".into(), "research".into(), CancellationToken::new());

    let input = b"{\"id\":\"1\",\"type\":\"subagent_list\"}\n{\"id\":\"2\",\"type\":\"subagent_steer\",\"agent_id\":\"sub-x\",\"message\":\"focus\"}\n{\"id\":\"3\",\"type\":\"subagent_kill\",\"agent_id\":\"sub-x\"}\n";
    let (writer, mut reader) = tokio::io::duplex(8192);
    run_rpc_mode(&mut session, BufReader::new(&input[..]), writer).await.unwrap();
    drop(session);

    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("sub-x"), "list response must include the child id");
    assert!(text.contains("\"ok\":true"), "steer and kill must report success");
    assert_eq!(child.steering_queue.lock().unwrap().len(), 1);
    assert!(child.cancel.is_cancelled());
    std::fs::remove_dir_all(dir).unwrap();
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent --test test_subagent_rpc`
Expected: unknown-command errors in the JSONL output.

- [ ] **Step 3: Implement three match arms in `rpc.rs`**

Follow the `"steer"` arm at `:463-471`. Read `agent_id` from `val.get("agent_id").and_then(|v| v.as_str())`; missing or unknown id yields `{"ok": false}` rather than an error. List uses `session.subagent_control().list()` and serializes each `SubagentSummary`.

- [ ] **Step 4: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent --test test_subagent_rpc`
Expected: test passes.

- [ ] **Step 5: Full crate regression and commit**

Run: `cargo test -p mehh-coding-agent`

```bash
git add crates/mehh-coding-agent/src/rpc.rs crates/mehh-coding-agent/tests/test_subagent_rpc.rs
git commit -m "feat(subagent): add subagent list, steer and kill RPC commands"
```

---

### Task 5: Nested subagent view in the interactive TUI

**Files:**
- Modify: `crates/mehh-coding-agent/src/interactive.rs:31-46` (TranscriptItem), tool-event arms at `:1697-1761` (the enclosing `apply_agent_event` starts near `:1625`), render arm at `:1917-1965`, tests near `:3195-3400` (`make_test_state` is at `:3117`)
- Test: `crates/mehh-coding-agent/src/interactive.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `AgentEvent::ToolExecutionStart/Update/End` and `AgentToolResult.details["subagent"]` from Task 2.
- Produces:
  - `struct SubagentChildRow { pub child_id: String, pub label: String, pub status: String, pub detail: String }`
  - `TranscriptItem::Subagent { id: String, mode: String, status: String, is_error: bool, children: Vec<SubagentChildRow> }`
  - `ToolExecutionStart` with `tool_name == "subagent"` creates `TranscriptItem::Subagent`; `mode` comes from `args["mode"]`, default `"research"`
  - `ToolExecutionUpdate` with `details["subagent"]` finds the item whose `id == tool_call_id`, then updates or inserts a row keyed by `child_id` (never by label)
  - `ToolExecutionEnd` sets the group's final `status` and `is_error`
  - Renderer prints `  ⚙ subagent [mode] (id): status`, then indented rows `    - <label>: <status> <detail>`

- [ ] **Step 1: Write failing tests in `interactive.rs` tests module**

```rust
#[test]
fn test_subagent_group_renders_nested_child_rows_keyed_by_tool_call_id() {
    let mut state = make_test_state();
    let tool_id = "tc_sub_1".to_string();

    apply_agent_event(&mut state, &AgentEvent::ToolExecutionStart {
        tool_call_id: tool_id.clone(),
        tool_name: "subagent".to_string(),
        args: serde_json::json!({"mode": "coding"}).as_object().unwrap().clone(),
    });
    assert!(matches!(state.items.last().unwrap(), TranscriptItem::Subagent { mode, .. } if mode == "coding"));

    let child_update = |id: &str, kind: &str, detail: &str| AgentEvent::ToolExecutionUpdate {
        tool_call_id: tool_id.clone(), tool_name: "subagent".to_string(), args: serde_json::Map::new(),
        partial_result: mehh_agent::AgentToolResult {
            details: Some(serde_json::json!({"subagent": {
                "child_id": id, "mode": "coding", "event_kind": kind, "detail": detail
            }})),
            ..Default::default()
        },
    };
    apply_agent_event(&mut state, &child_update("sub-a", "tool_started", "read"));
    apply_agent_event(&mut state, &child_update("sub-b", "tool_started", "read"));
    apply_agent_event(&mut state, &child_update("sub-b", "tool_finished", "read"));

    match state.items.last().unwrap() {
        TranscriptItem::Subagent { children, .. } => {
            assert_eq!(children.len(), 2);
            let a = children.iter().find(|r| r.child_id == "sub-a").unwrap();
            let b = children.iter().find(|r| r.child_id == "sub-b").unwrap();
            assert_eq!(a.status, "running");
            assert_eq!(b.status, "completed");
        }
        _ => panic!("expected subagent group"),
    }

    apply_agent_event(&mut state, &AgentEvent::ToolExecutionStart {
        tool_call_id: "tc_sub_2".to_string(), tool_name: "subagent".to_string(), args: serde_json::Map::new(),
    });
    assert_eq!(state.items.iter().filter(|i| matches!(i, TranscriptItem::Subagent { .. })).count(), 2);
}

#[test]
fn test_subagent_group_render_shows_child_rows() {
    let mut state = make_test_state();
    state.items.push(TranscriptItem::Subagent {
        id: "tc_x".into(), mode: "coding".into(), status: "running".into(), is_error: false,
        children: vec![SubagentChildRow {
            child_id: "sub-a".into(), label: "read".into(),
            status: "running".into(), detail: "src/main.rs".into(),
        }],
    });
    let editor = /* reuse make_test_editor pattern from test_render_interactive_frame */;
    let session = /* reuse make_test_session pattern */;
    let frame = render_interactive_frame(&state, &editor, &session, 80, 24).join("\n");
    assert!(frame.contains("subagent"), "header must name the tool");
    assert!(frame.contains("sub-a") || frame.contains("read"), "child row must be rendered");
}
```

- [ ] **Step 2: Run to confirm RED**

Run: `cargo test -p mehh-coding-agent test_subagent_group_renders_nested_child_rows_keyed_by_tool_call_id`
Expected: compile failure, `TranscriptItem::Subagent` missing.

- [ ] **Step 3: Implement the variant and event handling**

Add the struct/variant. In `apply_agent_event`, branch on `tool_name == "subagent"` in the three arms. For `ToolExecutionUpdate`, parse `partial_result.details.as_ref()?.get("subagent")?`; ignore updates without that key so unrelated tool updates still work. Insert or update a `SubagentChildRow` by `child_id`: `tool_started` → label = `detail`, status `running`; `tool_finished` → update that row's status to `completed` and keep its label; `finished`/`aborted`/`timed_out` → set group status. Keep the last 8 rows per group, same cap as today's `output_preview`.

- [ ] **Step 4: Implement rendering**

In the new `TranscriptItem::Subagent` arm of `render_interactive_frame`, print the header with existing theme tokens (`tool_pending`/`tool_success`/`tool_error`, `accent`, `dim`) using the same `wrap_text_with_ansi` call as `ToolCall`, then one indented line per child row. Use `⚙`, `✓`, `✗` by status exactly as the generic tool card does.

- [ ] **Step 5: Run to confirm GREEN**

Run: `cargo test -p mehh-coding-agent test_subagent_group`
Expected: both tests pass.

- [ ] **Step 6: Add failure-path write test**

Extend `tests/test_subagent_lifecycle.rs` with a test that sets `MEHH_AGENT_STORE` to a path whose parent is a regular file (so `create_dir_all` fails) and asserts `execute()` returns `Err` mentioning the store path rather than silently succeeding. Restore/remove the env var in the test. If parallel test execution makes env mutation unsafe, instead call `write_manifest` directly with an invalid directory and assert `Err`.

- [ ] **Step 7: Full crate regression and commit**

Run: `cargo test -p mehh-coding-agent`

```bash
git add crates/mehh-coding-agent/src/interactive.rs crates/mehh-coding-agent/tests/test_subagent_lifecycle.rs
git commit -m "feat(subagent): render nested subagent groups in interactive TUI"
```

---

## Final verification (after all tasks)

- [ ] `cargo test -p mehh-coding-agent` — all tests green
- [ ] `cargo check --workspace` — no type errors
- [ ] Manually run an interactive session, delegate a `coding` task, and confirm the nested group shows child tool rows in real time
- [ ] Confirm `<cwd>/.mehh-agents/<id>.json` contains a `started` event, child tool events, and a terminal event with `completed_at_ms` after the child finishes
