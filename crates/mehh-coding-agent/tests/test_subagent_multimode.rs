use mehh_coding_agent::subagent::SubagentTool;

#[test]
fn test_subagent_tool_parameters_support_tasks_and_modes() {
    let params = SubagentTool::parameters_schema();
    let mode_enum = params["properties"]["mode"]["enum"].as_array().expect("mode enum");
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
    assert!(research_tools.iter().all(|t| t.name() != "write" && t.name() != "edit" && t.name() != "bash"));

    let coding_tools = sub.filter_tools_for_mode("coding");
    assert!(coding_tools.iter().any(|t| t.name() == "edit"));
    assert!(coding_tools.iter().any(|t| t.name() == "write"));
    assert!(coding_tools.iter().all(|t| t.name() != "subagent" && t.name() != "codemode"));
}
