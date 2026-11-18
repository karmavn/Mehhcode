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
        SubagentStatus::Failed => failure_class
            .map(|class| class.as_str())
            .unwrap_or_else(|| SubagentFailureClass::Infra.as_str()),
    }
}

/// Directory where manifests are stored: `MEHH_AGENT_STORE` or `<cwd>/.mehh-agents`.
pub fn agent_store_dir(cwd: &Path) -> PathBuf {
    match std::env::var("MEHH_AGENT_STORE") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
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
