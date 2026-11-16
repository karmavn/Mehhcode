//! Deferred tool discovery and runtime activation into the agent loop's dynamic tools.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback};

/// Summary entry returned by [`DeferredToolCatalog::search`].
#[derive(Debug, Clone)]
pub struct DeferredToolMatch {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Catalog of deferred tools that can be searched and dynamically activated mid-session.
pub struct DeferredToolCatalog {
    deferred: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
    active_tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
}

impl DeferredToolCatalog {
    /// Creates a catalog wrapping `deferred` tools alongside an empty dynamic tool slot.
    pub fn new(deferred: Vec<Arc<dyn AgentTool>>) -> Self {
        Self {
            deferred: Arc::new(RwLock::new(deferred)),
            active_tools: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Replaces the registered deferred tools in the catalog.
    pub fn replace_deferred(&self, deferred: Vec<Arc<dyn AgentTool>>) {
        let mut guard = self.deferred.write().unwrap();
        *guard = deferred;
    }

    /// Returns the shared handle wired into `AgentLoopConfig.dynamic_tools`.
    pub fn dynamic_tools_handle(&self) -> Arc<RwLock<Vec<Arc<dyn AgentTool>>>> {
        Arc::clone(&self.active_tools)
    }

    /// Returns the number of deferred tools registered in the catalog.
    pub fn len(&self) -> usize {
        self.deferred.read().unwrap().len()
    }

    /// Returns true if no deferred tools are registered.
    pub fn is_empty(&self) -> bool {
        self.deferred.read().unwrap().is_empty()
    }

    /// Searches deferred tools by case-insensitive keyword matching against names and descriptions.
    pub fn search(&self, query: &str, max_results: usize) -> Vec<Arc<dyn AgentTool>> {
        let max_results = max_results.clamp(1, 25);
        let q = query.trim().to_ascii_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();

        let deferred_guard = self.deferred.read().unwrap();
        let mut scored: Vec<(usize, &Arc<dyn AgentTool>)> = deferred_guard
            .iter()
            .filter_map(|tool| {
                if terms.is_empty() {
                    return Some((1, tool));
                }
                let name_lower = tool.name().to_ascii_lowercase();
                let desc_lower = tool.description().to_ascii_lowercase();
                let mut score = 0usize;
                for term in &terms {
                    if name_lower == *term {
                        score += 100;
                    } else if name_lower.contains(term) {
                        score += 40;
                    } else if desc_lower.contains(term) {
                        score += 10;
                    } else {
                        return None;
                    }
                }
                Some((score, tool))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name().cmp(b.1.name())));
        scored
            .into_iter()
            .take(max_results)
            .map(|(_, tool)| Arc::clone(tool))
            .collect()
    }

    /// Activates the given tools inside `active_tools` without creating duplicates.
    /// Returns the names of tools newly added on this call.
    pub fn activate(&self, tools: &[Arc<dyn AgentTool>]) -> Vec<String> {
        let mut guard = self.active_tools.write().unwrap();
        let mut added = Vec::new();
        for tool in tools {
            let name = tool.name();
            if !guard.iter().any(|existing| existing.name() == name) {
                guard.push(Arc::clone(tool));
                added.push(name.to_string());
            }
        }
        added
    }

    /// Activates deferred tools matching explicit names. Returns `(activated, not_found)`.
    pub fn activate_by_names(&self, names: &[String]) -> (Vec<String>, Vec<String>) {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        let deferred_guard = self.deferred.read().unwrap();
        for name in names {
            if let Some(tool) = deferred_guard
                .iter()
                .find(|t| t.name().eq_ignore_ascii_case(name))
            {
                found.push(Arc::clone(tool));
            } else {
                missing.push(name.clone());
            }
        }
        let activated = self.activate(&found);
        (activated, missing)
    }
}

/// Built-in `tool_search` tool for discovering and enabling deferred tools at runtime.
pub struct ToolSearchTool {
    catalog: Arc<DeferredToolCatalog>,
}

impl ToolSearchTool {
    pub fn new(catalog: Arc<DeferredToolCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl AgentTool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Search deferred MCP or extension tools by keyword and activate matching tools for subsequent turns."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search keywords matching tool name or description (optional if names is supplied)."
                },
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exact tool names to activate immediately."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 25,
                    "description": "Maximum number of tools to return (default 5)."
                },
                "activate": {
                    "type": "boolean",
                    "description": "Whether to register matching tools for use on the next turn (default true)."
                }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let activate = params
            .get("activate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let max_results = params
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        if let Some(names_val) = params.get("names").and_then(|v| v.as_array()) {
            let names: Vec<String> = names_val
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            let (activated, missing) = self.catalog.activate_by_names(&names);
            return Ok(AgentToolResult::text(
                serde_json::to_string_pretty(&json!({
                    "activated": activated,
                    "notFound": missing,
                }))
                .unwrap_or_default(),
            ));
        }

        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let matches = self.catalog.search(query, max_results);
        let newly_activated = if activate {
            self.catalog.activate(&matches)
        } else {
            Vec::new()
        };

        let entries: Vec<Value> = matches
            .iter()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                })
            })
            .collect();

        Ok(AgentToolResult::text(
            serde_json::to_string_pretty(&json!({
                "matches": entries,
                "newlyActivated": newly_activated,
            }))
            .unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDeferredTool(&'static str, &'static str);

    #[async_trait]
    impl AgentTool for FakeDeferredTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            self.1
        }
        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _id: &str,
            _params: Map<String, Value>,
            _cancel: CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            Ok(AgentToolResult::text("ok"))
        }
    }

    #[tokio::test]
    async fn test_tool_search_finds_and_activates_deferred_tools() {
        let deferred: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(FakeDeferredTool("mcp__gh__list_issues", "List GitHub repository issues")),
            Arc::new(FakeDeferredTool("mcp__slack__post_msg", "Send message to Slack channel")),
        ];
        let catalog = Arc::new(DeferredToolCatalog::new(deferred));
        let handle = catalog.dynamic_tools_handle();
        assert!(handle.read().unwrap().is_empty());

        let tool = ToolSearchTool::new(Arc::clone(&catalog));
        let mut params = Map::new();
        params.insert("query".to_string(), Value::String("github issues".to_string()));
        let res = tool
            .execute("tc_1", params, CancellationToken::new(), None)
            .await
            .unwrap();

        let text = match &res.content[0] {
            mehh_ai::ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!("Expected text output"),
        };
        assert!(text.contains("mcp__gh__list_issues"));
        assert!(!text.contains("mcp__slack__post_msg"));

        let active = handle.read().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name(), "mcp__gh__list_issues");
    }
}
