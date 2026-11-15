//! Depth-bounded, read-only subagent tool for delegated research tasks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use mehh_agent::{
    run_agent_loop, AgentLoopConfig, AgentTool, AgentToolResult, AgentToolUpdateCallback,
};
use mehh_ai::{now_ms, LlmProvider, Message, Model, StopReason, UserContent, UserMessage};

/// Maximum nested subagent recursion depth.
pub const MAX_SUBAGENT_DEPTH: usize = 2;

/// Maximum assistant turns allowed per subagent invocation.
pub const SUBAGENT_MAX_TURNS: usize = 20;

/// Maximum wall-clock timeout for a subagent invocation in seconds.
pub const SUBAGENT_TIMEOUT_SECS: u64 = 180;

/// Runs a delegated subagent in an isolated context with bounded turns and read-only tools.
pub struct SubagentTool {
    pub cwd: PathBuf,
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub api_key: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub depth: usize,
}

impl SubagentTool {
    pub fn new(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        api_key: Option<String>,
        tools: Vec<Arc<dyn AgentTool>>,
        depth: usize,
    ) -> Self {
        Self {
            cwd,
            model,
            provider,
            api_key,
            tools,
            depth,
        }
    }

    fn child_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools
            .iter()
            .filter(|t| matches!(t.name(), "read" | "grep" | "find" | "ls"))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Delegate an isolated, read-only research task to a bounded subagent and return its final summary."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Self-contained task description for the subagent."
                },
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system instructions for the subagent."
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "description": "Optional turn budget override (capped at 20)."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Err(format!(
                "Maximum subagent nesting depth ({MAX_SUBAGENT_DEPTH}) reached"
            ));
        }

        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'prompt' parameter".to_string())?;

        let custom_sys = params
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let max_turns = params
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, SUBAGENT_MAX_TURNS))
            .unwrap_or(SUBAGENT_MAX_TURNS);

        let mut config = AgentLoopConfig::new(self.model.clone(), Arc::clone(&self.provider));
        config.cancel = cancel.child_token();
        config.max_turns = Some(max_turns);
        config.tools = self.child_tools();
        config.system_prompt = Some(custom_sys.unwrap_or_else(|| {
            format!(
                "You are a focused read-only research subagent working in {}. Investigate the request and report concise findings with exact file paths.",
                self.cwd.display()
            )
        }));

        if let Some(ref key) = self.api_key {
            config.stream_options.api_key = Some(key.clone());
        } else if self.model.provider != "faux" && self.model.api != "faux" {
            if let Ok(Some(key)) =
                crate::auth::resolve_api_key_async(&self.model.provider, None, None).await
            {
                config.stream_options.api_key = Some(key);
            }
        }

        let user_msg = Message::User(UserMessage {
            content: UserContent::Text(prompt.to_string()),
            timestamp: now_ms(),
        });

        let messages = tokio::time::timeout(
            Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
            run_agent_loop(vec![user_msg], Vec::new(), config, None),
        )
        .await
        .map_err(|_| format!("Subagent timed out after {SUBAGENT_TIMEOUT_SECS}s"))?;

        let final_ast = messages.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        });

        match final_ast {
            Some(a) if a.stop_reason == StopReason::Error => Err(a
                .error_message
                .clone()
                .unwrap_or_else(|| "Subagent model call failed".to_string())),
            Some(a) if a.stop_reason == StopReason::Aborted => {
                Err("Subagent execution was aborted".to_string())
            }
            Some(a) => Ok(AgentToolResult::text(a.text_content())),
            None => Err("Subagent produced no assistant response".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{faux_assistant_message, faux_model, FauxProvider};

    #[tokio::test]
    async fn test_subagent_runs_isolated_prompt_and_respects_max_depth() {
        let faux = Arc::new(FauxProvider::new());
        faux.add_response(faux_assistant_message("Found 3 matching files."));

        let cwd = std::env::temp_dir();
        let tools = mehh_tools::create_coding_tools(cwd.clone());
        let sub = SubagentTool::new(cwd.clone(), faux_model(), faux.clone(), None, tools.clone(), 0);
        assert!(sub.child_tools().iter().all(|t| matches!(t.name(), "read" | "grep" | "find" | "ls")));

        let mut params = Map::new();
        params.insert("prompt".to_string(), Value::String("Inspect crates".to_string()));
        let res = sub
            .execute("tc_sub_1", params.clone(), CancellationToken::new(), None)
            .await
            .unwrap();
        let text = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!("Expected text output"),
        };
        assert_eq!(text, "Found 3 matching files.");

        // Depth at MAX_SUBAGENT_DEPTH fails closed
        let deep = SubagentTool::new(cwd, faux_model(), faux, None, tools, MAX_SUBAGENT_DEPTH);
        let err = deep
            .execute("tc_sub_2", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("Maximum subagent nesting depth"));
    }
}
