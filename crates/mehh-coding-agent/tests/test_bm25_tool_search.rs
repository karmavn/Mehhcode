use mehh_coding_agent::tool_search::DeferredToolCatalog;

#[test]
fn test_bm25_ranks_schema_property_matches_higher_than_noise() {
    let mut catalog = DeferredToolCatalog::new(Vec::new());
    catalog.register_test_tool(
        "database_query",
        "Execute SQL",
        serde_json::json!({
            "properties": { "table_name": { "description": "Target SQL table" } }
        }),
    );
    catalog.register_test_tool("unrelated_tool", "Unrelated utility", serde_json::json!({}));

    let results = catalog.search("table_name", 5);
    assert!(!results.is_empty(), "expected match on table_name property");
    assert_eq!(results[0].name(), "database_query");
}
