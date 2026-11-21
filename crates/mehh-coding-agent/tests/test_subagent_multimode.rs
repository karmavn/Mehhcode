use mehh_coding_agent::subagent::SubagentTool;

/// Probe tool with configurable name and read-only annotation.
struct ProbeTool {
    name: &'static str,
    read_only: Option<bool>,
}

#[async_trait::async_trait]
impl mehh_agent::AgentTool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "probe"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn is_read_only(&self) -> Option<bool> {
        self.read_only
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Map<String, serde_json::Value>,
        _cancel: tokio_util::sync::CancellationToken,
        _on_update: Option<mehh_agent::AgentToolUpdateCallback>,
    ) -> Result<mehh_agent::AgentToolResult, String> {
        Ok(mehh_agent::AgentToolResult::text("probe"))
    }
}

#[test]
fn test_subagent_tool_parameters_support_tasks_and_modes() {
    let params = SubagentTool::parameters_schema();
    let mode_enum = params["properties"]["mode"]["enum"]
        .as_array()
        .expect("mode enum");
    assert!(mode_enum.contains(&serde_json::json!("research")));
    assert!(mode_enum.contains(&serde_json::json!("coding")));
    assert!(mode_enum.contains(&serde_json::json!("review")));
    assert!(params["properties"]["tasks"].is_object());
}

#[tokio::test]
async fn test_subagent_mode_tool_filtering() {
    let cwd = std::env::temp_dir();
    let tools = mehh_tools::create_coding_tools(cwd.clone());
    let faux = std::sync::Arc::new(mehh_ai::FauxProvider::new());
    let sub = SubagentTool::new(cwd, mehh_ai::faux_model(), faux, None, tools, 0);

    let research_tools = sub.filter_tools_for_mode("research");
    assert!(research_tools.iter().any(|t| t.name() == "read"));
    assert!(research_tools
        .iter()
        .all(|t| t.name() != "write" && t.name() != "edit" && t.name() != "bash"));

    let coding_tools = sub.filter_tools_for_mode("coding");
    assert!(coding_tools.iter().any(|t| t.name() == "edit"));
    assert!(coding_tools.iter().any(|t| t.name() == "write"));
    assert!(coding_tools
        .iter()
        .all(|t| t.name() != "subagent" && t.name() != "codemode"));
}

#[test]
fn test_readonly_modes_filter_by_capability_not_name() {
    let cwd = std::env::temp_dir();
    let mut tools = mehh_tools::create_coding_tools(cwd.clone());
    // A read-only MCP-style tool with an arbitrary name must be included.
    tools.push(std::sync::Arc::new(ProbeTool {
        name: "docs_search",
        read_only: Some(true),
    }));
    // Unannotated tools must fail closed (excluded).
    tools.push(std::sync::Arc::new(ProbeTool {
        name: "mystery",
        read_only: None,
    }));
    // Recursive/stateful tools stay excluded even if they claim read-only.
    tools.push(std::sync::Arc::new(ProbeTool {
        name: "todo",
        read_only: Some(true),
    }));
    tools.push(std::sync::Arc::new(ProbeTool {
        name: "subagent",
        read_only: Some(true),
    }));
    tools.push(std::sync::Arc::new(ProbeTool {
        name: "codemode",
        read_only: Some(true),
    }));

    let faux = std::sync::Arc::new(mehh_ai::FauxProvider::new());
    let sub = SubagentTool::new(cwd, mehh_ai::faux_model(), faux, None, tools, 0);

    for mode in ["research", "review"] {
        let filtered = sub.filter_tools_for_mode(mode);
        let names: Vec<&str> = filtered.iter().map(|t| t.name()).collect();
        assert!(
            names.contains(&"docs_search"),
            "{mode}: read-only MCP tool must pass, got {names:?}"
        );
        assert!(
            names.contains(&"read") && names.contains(&"grep"),
            "{mode}: builtin readers kept"
        );
        assert!(
            !names.contains(&"mystery"),
            "{mode}: unannotated tool must fail closed"
        );
        assert!(!names.contains(&"write") && !names.contains(&"edit") && !names.contains(&"bash"));
        assert!(
            !names.contains(&"todo")
                && !names.contains(&"subagent")
                && !names.contains(&"codemode")
        );
    }
}
