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

fn tokenize_terms(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for chunk in text.split(|c: char| c.is_whitespace() || c == '_' || c == '-' || c == ':' || c == '.') {
        if chunk.is_empty() {
            continue;
        }
        let mut curr = String::new();
        for ch in chunk.chars() {
            if ch.is_uppercase() && !curr.is_empty() {
                tokens.push(curr);
                curr = String::new();
            }
            curr.push(ch.to_ascii_lowercase());
        }
        if !curr.is_empty() {
            tokens.push(curr);
        }
    }
    tokens
}

fn extract_schema_terms(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::Object(map) => {
            for (k, v) in map {
                out.push(k.clone());
                if k == "description" {
                    if let Some(desc_str) = v.as_str() {
                        out.push(desc_str.to_string());
                    }
                } else {
                    extract_schema_terms(v, out);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                extract_schema_terms(item, out);
            }
        }
        _ => {}
    }
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

    /// Helper to register a test tool into the deferred catalog.
    pub fn register_test_tool(
        &mut self,
        name: &'static str,
        desc: &'static str,
        parameters: Value,
    ) {
        struct TestDeferredTool {
            name: &'static str,
            desc: &'static str,
            params: Value,
        }
        #[async_trait]
        impl AgentTool for TestDeferredTool {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                self.desc
            }
            fn parameters(&self) -> Value {
                self.params.clone()
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

        let mut guard = self.deferred.write().unwrap();
        guard.push(Arc::new(TestDeferredTool {
            name,
            desc,
            params: parameters,
        }));
    }

    /// Searches deferred tools using Okapi BM25 (k1=1.2, b=0.75) across names, descriptions,
    /// and JSON schema parameters, with exact name and substring boosts.
    pub fn search(&self, query: &str, max_results: usize) -> Vec<Arc<dyn AgentTool>> {
        let max_results = max_results.clamp(1, 25);
        let q = query.trim();
        let query_norm = q.to_ascii_lowercase();
        let query_tokens = tokenize_terms(&query_norm);

        let deferred_guard = self.deferred.read().unwrap();
        if deferred_guard.is_empty() {
            return Vec::new();
        }
        if query_tokens.is_empty() {
            return deferred_guard.iter().take(max_results).map(Arc::clone).collect();
        }

        let num_docs = deferred_guard.len() as f64;
        const K1: f64 = 1.2;
        const B: f64 = 0.75;

        // Build document representations
        struct DocRep<'a> {
            tool: &'a Arc<dyn AgentTool>,
            tf_map: std::collections::HashMap<String, f64>,
            doc_len: f64,
        }

        let mut docs = Vec::with_capacity(deferred_guard.len());
        let mut total_len = 0.0;

        for tool in deferred_guard.iter() {
            let mut tf_map = std::collections::HashMap::new();
            let mut doc_len = 0.0;

            // Name tokens weighted 3.0
            for tok in tokenize_terms(tool.name()) {
                *tf_map.entry(tok).or_insert(0.0) += 3.0;
                doc_len += 3.0;
            }

            // Description tokens weighted 1.0
            for tok in tokenize_terms(tool.description()) {
                *tf_map.entry(tok).or_insert(0.0) += 1.0;
                doc_len += 1.0;
            }

            // Parameter schema tokens weighted 1.0
            let mut schema_raw = Vec::new();
            extract_schema_terms(&tool.parameters(), &mut schema_raw);
            for s in schema_raw {
                for tok in tokenize_terms(&s) {
                    *tf_map.entry(tok).or_insert(0.0) += 1.0;
                    doc_len += 1.0;
                }
            }

            total_len += doc_len;
            docs.push(DocRep {
                tool,
                tf_map,
                doc_len,
            });
        }

        let avgdl = if num_docs > 0.0 {
            total_len / num_docs
        } else {
            1.0
        };

        // Precompute IDF for each query token across docs (reduces complexity from O(D^2 * T) to O(D * T))
        let idfs: Vec<(&String, f64)> = query_tokens
            .iter()
            .filter_map(|q_term| {
                let df = docs
                    .iter()
                    .filter(|d| d.tf_map.contains_key(q_term))
                    .count() as f64;
                if df > 0.0 {
                    let idf = ((num_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                    Some((q_term, idf))
                } else {
                    None
                }
            })
            .collect();

        let mut scored: Vec<(f64, &Arc<dyn AgentTool>)> = Vec::with_capacity(docs.len());

        for doc in &docs {
            let mut score = 0.0f64;

            for &(q_term, idf) in &idfs {
                if let Some(&tf) = doc.tf_map.get(q_term) {
                    let numerator = tf * (K1 + 1.0);
                    let denominator = tf + K1 * (1.0 - B + B * (doc.doc_len / avgdl));
                    score += idf * (numerator / denominator);
                }
            }

            let tool_name_lower = doc.tool.name().to_ascii_lowercase();
            if tool_name_lower == query_norm {
                score += 50.0;
            } else if tool_name_lower.contains(&query_norm) {
                score += 15.0;
            }

            if score > 0.001 {
                scored.push((score, doc.tool));
            }
        }

        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.name().cmp(b.1.name()))
        });

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
