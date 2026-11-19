//! Machine-readable lifecycle manifests for subagent runs.
//!
//! Each subagent writes one JSON manifest so that display, diagnostics, and
//! tooling can read structured state instead of parsing free-form text.
//! Timestamps are integer epoch milliseconds with `_ms` field names.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Terminal or in-flight status of a subagent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
}

/// Kinds of timeline entries recorded in a manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentEventKind {
    Started,
    ToolStarted,
    ToolFinished,
    Blocked,
    Finished,
    Aborted,
    TimedOut,
}

/// One timeline entry for a subagent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentEvent {
    pub kind: SubagentEventKind,
    /// Epoch milliseconds when the event happened.
    pub at_ms: i64,
    /// Optional human-readable detail such as a tool name or error snippet.
    pub detail: Option<String>,
}

/// Coarse failure bucket for a failed subagent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentFailureClass {
    Transport,
    ToolRuntime,
    Compile,
    Test,
    Mcp,
    Timeout,
    Aborted,
    Infra,
}

impl SubagentFailureClass {
    /// Snake-case name used in manifests and derived state.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::ToolRuntime => "tool_runtime",
            Self::Compile => "compile",
            Self::Test => "test",
            Self::Mcp => "mcp",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
            Self::Infra => "infra",
        }
    }
}

/// Persistent record of one subagent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentManifest {
    pub agent_id: String,
    pub mode: String,
    pub model_id: String,
    pub status: SubagentStatus,
    /// Epoch milliseconds when the run was created.
    pub created_at_ms: i64,
    /// Epoch milliseconds when the run reached a terminal state.
    pub completed_at_ms: Option<i64>,
    pub events: Vec<SubagentEvent>,
    pub failure_class: Option<SubagentFailureClass>,
    pub derived_state: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Generates a sortable, collision-free subagent id.
pub fn new_agent_id() -> String {
    format!("sub-{}", uuid::Uuid::now_v7())
}

/// Builds the initial manifest for a subagent that has just started.
pub fn new_manifest(
    agent_id: String,
    mode: String,
    model_id: String,
    now_ms: i64,
) -> SubagentManifest {
    SubagentManifest {
        agent_id,
        mode,
        model_id,
        status: SubagentStatus::Running,
        created_at_ms: now_ms,
        completed_at_ms: None,
        events: vec![SubagentEvent {
            kind: SubagentEventKind::Started,
            at_ms: now_ms,
            detail: None,
        }],
        failure_class: None,
        derived_state: "working".to_string(),
        result: None,
        error: None,
    }
}

/// Maps an error string to a coarse failure class.
pub fn classify_failure(error: &str) -> SubagentFailureClass {
    let lower = error.to_lowercase();
    if lower.contains("timed out") {
        SubagentFailureClass::Timeout
    } else if lower.contains("aborted") {
        SubagentFailureClass::Aborted
    } else if lower.contains("compile") || lower.contains("build failed") {
        SubagentFailureClass::Compile
    } else if lower.contains("test") {
        SubagentFailureClass::Test
    } else if lower.contains("tool runtime") || lower.contains("tool failed") {
        SubagentFailureClass::ToolRuntime
    } else if lower.contains("mcp") {
        SubagentFailureClass::Mcp
    } else if lower.contains("transport") || lower.contains("pipe") || lower.contains("connection")
    {
        SubagentFailureClass::Transport
    } else {
        SubagentFailureClass::Infra
    }
}

/// Derives the manifest's short state label from status and failure class.
pub fn derive_state(
    status: &SubagentStatus,
    failure_class: Option<&SubagentFailureClass>,
) -> &'static str {
    match status {
        SubagentStatus::Running => "working",
        SubagentStatus::Completed => "finished_cleanable",
        // Without a failure_class there is no bucket to name; report the real
        // status instead of masking it behind a guessed "infra".
        SubagentStatus::Failed => failure_class
            .map(|class| class.as_str())
            .unwrap_or("failed"),
    }
}

/// Directory where manifests are stored: `MEHH_AGENT_STORE` or `<cwd>/.mehh-agents`.
///
/// Reads the override as an `OsString` so non-UTF8 paths (legal on Windows)
/// are honored instead of silently falling back to the default directory.
pub fn agent_store_dir(cwd: &Path) -> PathBuf {
    match std::env::var_os("MEHH_AGENT_STORE") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => cwd.join(".mehh-agents"),
    }
}

/// Writes a manifest as pretty JSON to `<dir>/<agent_id>.json`.
pub fn write_manifest(dir: &Path, manifest: &SubagentManifest) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let path = dir.join(format!("{}.json", manifest.agent_id));
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|err| format!("failed to serialize manifest: {err}"))?;
    std::fs::write(&path, json)
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(path)
}

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
        assert_eq!(
            classify_failure("tool runtime exploded"),
            SubagentFailureClass::ToolRuntime
        );
        assert_eq!(
            classify_failure("compile failed"),
            SubagentFailureClass::Compile
        );
        assert_eq!(
            classify_failure("broken pipe"),
            SubagentFailureClass::Transport
        );
        assert_eq!(
            classify_failure("Subagent timed out after 180s"),
            SubagentFailureClass::Timeout
        );
        assert_eq!(classify_failure("no idea"), SubagentFailureClass::Infra);
    }

    #[test]
    fn derive_state_failed_without_failure_class_reports_failed() {
        // A Failed status with no failure_class must not be relabeled "infra":
        // that hid the real status from consumers reading derived_state.
        assert_eq!(derive_state(&SubagentStatus::Failed, None), "failed");
        assert_eq!(
            derive_state(&SubagentStatus::Failed, Some(&SubagentFailureClass::Timeout)),
            "timeout"
        );
        assert_eq!(derive_state(&SubagentStatus::Running, None), "working");
        assert_eq!(
            derive_state(&SubagentStatus::Completed, None),
            "finished_cleanable"
        );
    }

    /// Sets `MEHH_AGENT_STORE` and restores the previous value on drop so
    /// sibling tests in this binary never observe a poisoned store path.
    struct StoreEnvGuard {
        prev: Option<std::ffi::OsString>,
        store: std::path::PathBuf,
    }

    impl StoreEnvGuard {
        fn set(store: std::path::PathBuf) -> Self {
            let prev = std::env::var_os("MEHH_AGENT_STORE");
            unsafe { std::env::set_var("MEHH_AGENT_STORE", &store) };
            Self { prev, store }
        }
    }

    impl Drop for StoreEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("MEHH_AGENT_STORE", v),
                    None => std::env::remove_var("MEHH_AGENT_STORE"),
                }
            }
            // A concurrent sibling may have written a manifest through the
            // override while this test held it; clean that up too.
            let _ = std::fs::remove_dir_all(&self.store);
        }
    }

    /// Absolute temp path whose file name is not valid UTF-8 on this platform.
    fn non_utf8_store_path() -> std::path::PathBuf {
        #[cfg(windows)]
        let component = {
            use std::os::windows::ffi::OsStringExt;
            // Unpaired surrogate: a valid OsString on Windows that is not
            // valid UTF-8, the case `std::env::var` rejects with NotUnicode.
            std::ffi::OsString::from_wide(&[0x006D, 0xD800, 0x0065])
        };
        #[cfg(not(windows))]
        let component = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(b"mehh-\xFF-store".to_vec())
        };
        std::env::temp_dir().join(format!("mehh_store_{}", uuid::Uuid::now_v7())).join(component)
    }

    #[test]
    fn agent_store_dir_honors_non_utf8_env_override() {
        // Only `test_subagent_runs_isolated_prompt_and_respects_max_depth`
        // in this binary resolves the store dir concurrently, and it makes no
        // assertion about where its manifest lands, so the brief override
        // window is safe.
        let cwd = std::path::Path::new("/proj/work");
        let store = non_utf8_store_path();
        let _guard = StoreEnvGuard::set(store.clone());
        assert_eq!(agent_store_dir(cwd), store);

        // An empty override falls back to `<cwd>/.mehh-agents`.
        unsafe { std::env::set_var("MEHH_AGENT_STORE", "") };
        assert_eq!(agent_store_dir(cwd), cwd.join(".mehh-agents"));
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
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(raw["status"], "failed");
        assert_eq!(raw["created_at_ms"], 1000);
        assert_eq!(raw["completed_at_ms"], 2500);
        assert_eq!(raw["failure_class"], "test");
        assert!(raw.get("iso8601_now").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
