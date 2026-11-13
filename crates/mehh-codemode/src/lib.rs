pub mod sandbox;
pub mod tool;

pub use sandbox::*;
pub use tool::*;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::future::BoxFuture;
    use mehh_agent::{tool_to_definition, AgentTool, AgentToolResult, AgentToolUpdateCallback};
    use mehh_ai::ToolResultContentBlock;
    use tokio_util::sync::CancellationToken;

    use crate::sandbox::{
        run_codemode, CodemodeConfig, CodemodeOutputItem, SandboxToolExecutor,
    };
    use crate::tool::CodemodeTool;

    struct MockEchoTool;

    #[async_trait]
    impl AgentTool for MockEchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes the provided message."
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                }
            })
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            params: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            let msg = params.get("msg").and_then(|v| v.as_str()).unwrap_or("");
            Ok(AgentToolResult::text(format!("echo: {msg}")))
        }
    }

    fn dummy_executor() -> SandboxToolExecutor {
        Arc::new(|_name: &str, _args: serde_json::Value| -> BoxFuture<'static, Result<serde_json::Value, String>> {
            Box::pin(async { Err("dummy executor".to_string()) })
        })
    }

    // 1. Basic arithmetic + return + console.log + text().
    #[tokio::test]
    async fn test_basic_arithmetic_return_console_text() {
        let code = r#"
            console.log("hello from console");
            console.warn("warning here");
            text("some direct text");
            return 2 + 3 * 4;
        "#;
        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();
        let result = run_codemode(
            code,
            &[],
            dummy_executor(),
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;

        assert!(result.ok, "Expected ok: true, got error: {:?}", result.error);
        assert_eq!(result.value, Some(serde_json::json!(14)));
        assert_eq!(result.error, None);

        let has_log = result.items.iter().any(|item| {
            matches!(item, CodemodeOutputItem::Console { level, text } if level == "log" && text == "hello from console")
        });
        let has_warn = result.items.iter().any(|item| {
            matches!(item, CodemodeOutputItem::Console { level, text } if level == "warn" && text == "warning here")
        });
        let has_text = result.items.iter().any(|item| {
            matches!(item, CodemodeOutputItem::Text(t) if t == "some direct text")
        });

        assert!(has_log, "Missing console.log output");
        assert!(has_warn, "Missing console.warn output");
        assert!(has_text, "Missing text() output");
    }

    // 2. Top-level await tools.echo({ msg: "hi" }) calling an async mock AgentTool.
    #[tokio::test]
    async fn test_top_level_await_mock_tool() {
        let echo_tool = Arc::new(MockEchoTool);
        let tool_def = tool_to_definition(echo_tool.as_ref());
        let tool_clone = echo_tool.clone();

        let executor: SandboxToolExecutor = Arc::new(move |_name: &str, args: serde_json::Value| {
            let tool = tool_clone.clone();
            Box::pin(async move {
                let params = match args {
                    serde_json::Value::Object(m) => m,
                    _ => serde_json::Map::new(),
                };
                let res = tool.execute("call_1", params, CancellationToken::new(), None).await?;
                let text = res
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(serde_json::Value::String(text))
            })
        });

        let code = r#"
            const res = await tools.echo({ msg: "hi" });
            return { echoed: res };
        "#;
        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();
        let result = run_codemode(
            code,
            &[tool_def],
            executor,
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;

        assert!(result.ok, "Expected ok: true, got: {:?}", result.error);
        assert_eq!(
            result.value,
            Some(serde_json::json!({ "echoed": "echo: hi" }))
        );
    }

    // 3. Unknown tool call throws clear error with tool name.
    #[tokio::test]
    async fn test_unknown_tool_call_throws_error_with_name() {
        let echo_tool = Arc::new(MockEchoTool);
        let tool_def = tool_to_definition(echo_tool.as_ref());

        let code = r#"
            await tools.nonexistent({ data: 123 });
        "#;
        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();
        let result = run_codemode(
            code,
            &[tool_def],
            dummy_executor(),
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;

        assert!(!result.ok, "Expected ok: false for unknown tool");
        let err = result.error.expect("Expected error message");
        assert!(
            err.contains("nonexistent"),
            "Error message should mention 'nonexistent', got: {err}"
        );
        assert!(
            err.contains("does not exist"),
            "Error message should say 'does not exist', got: {err}"
        );
    }

    // 4. store("k", { a: 1 }) + load("k") persistence across two runs.
    #[tokio::test]
    async fn test_store_load_persistence_across_runs() {
        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();

        let run1_code = r#"
            store("k", { a: 1, b: "hello" });
        "#;
        let res1 = run_codemode(
            run1_code,
            &[],
            dummy_executor(),
            kv.clone(),
            &config,
            CancellationToken::new(),
        )
        .await;
        assert!(res1.ok, "Run 1 failed: {:?}", res1.error);

        let run2_code = r#"
            const val = load("k");
            return val;
        "#;
        let res2 = run_codemode(
            run2_code,
            &[],
            dummy_executor(),
            kv.clone(),
            &config,
            CancellationToken::new(),
        )
        .await;
        assert!(res2.ok, "Run 2 failed: {:?}", res2.error);
        assert_eq!(
            res2.value,
            Some(serde_json::json!({ "a": 1, "b": "hello" }))
        );
    }

    // 5. Infinite loop (while (true) {}) hits timeout interrupt cleanly without hanging.
    #[tokio::test]
    async fn test_infinite_loop_timeout_interrupt() {
        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig {
            timeout: Duration::from_millis(100),
            ..Default::default()
        };

        let code = r#"
            while (true) {}
        "#;

        let start = std::time::Instant::now();
        let result = run_codemode(
            code,
            &[],
            dummy_executor(),
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(!result.ok, "Expected ok: false for timed out script");
        let err = result.error.unwrap_or_default();
        assert!(
            err.to_lowercase().contains("time") || err.to_lowercase().contains("timed out") || err.to_lowercase().contains("interrupted"),
            "Error should indicate timeout, got: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "Infinite loop took too long to abort: {elapsed:?}"
        );
    }

    // 6. CodemodeTool end-to-end execution as an AgentTool.
    #[tokio::test]
    async fn test_codemode_tool_end_to_end() {
        let echo_tool = Arc::new(MockEchoTool);
        let codemode = CodemodeTool::new(vec![echo_tool]);

        let mut params = serde_json::Map::new();
        params.insert(
            "code".to_string(),
            serde_json::Value::String(
                r#"
                const res = await tools.echo({ msg: "world" });
                text("Greeting: " + res);
                return { done: true };
            "#
                .to_string(),
            ),
        );

        let res = codemode
            .execute("call_123", params, CancellationToken::new(), None)
            .await;

        assert!(res.is_ok(), "Expected execute to succeed: {:?}", res.err());
        let tool_result = res.unwrap();

        let has_greeting = tool_result.content.iter().any(|b| match b {
            ToolResultContentBlock::Text(t) => t.text.contains("Greeting: echo: world"),
            _ => false,
        });
        assert!(has_greeting, "Result should contain greeting text block");

        let has_return = tool_result.content.iter().any(|b| match b {
            ToolResultContentBlock::Text(t) => t.text.contains("Return value:"),
            _ => false,
        });
        assert!(has_return, "Result should contain return value");

        assert_eq!(tool_result.details, Some(serde_json::json!({ "done": true })));
    }

    struct MockNamedTool {
        name: String,
        description: String,
        parameters: serde_json::Value,
    }

    #[async_trait]
    impl AgentTool for MockNamedTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn parameters(&self) -> serde_json::Value {
            self.parameters.clone()
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _on_update: Option<AgentToolUpdateCallback>,
        ) -> Result<AgentToolResult, String> {
            Ok(AgentToolResult::text("ok"))
        }
    }

    // 7. Verify await searchTools("edit") and await describeTool("edit") in QuickJS.
    #[tokio::test]
    async fn test_search_and_describe_tool() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(MockNamedTool {
                name: "edit".to_string(),
                description: "Edits an existing file with replacement text.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            }),
            Arc::new(MockNamedTool {
                name: "read_file".to_string(),
                description: "Reads the content of a file from disk.".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }),
            Arc::new(MockNamedTool {
                name: "mcp__gh__issue_list".to_string(),
                description: "Lists issues for a GitHub repository.".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }),
        ];

        let tool_defs: Vec<mehh_ai::ToolDefinition> =
            tools.iter().map(|t| tool_to_definition(t.as_ref())).collect();

        let code = r#"
            const searchRes = await searchTools("edit");
            const searchGh = await searchTools("issue", { namespace: "gh" });
            const desc = await describeTool("edit");
            const missing = await describeTool("non_existent_tool");
            return { searchRes, searchGh, desc, missing };
        "#;

        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();
        let result = run_codemode(
            code,
            &tool_defs,
            dummy_executor(),
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;

        assert!(result.ok, "Expected run_codemode to succeed: {:?}", result.error);
        let val = result.value.expect("Expected result value");

        let search_res = val.get("searchRes").and_then(|v| v.as_array()).unwrap();
        assert!(!search_res.is_empty());
        assert_eq!(search_res[0].get("name").and_then(|n| n.as_str()), Some("edit"));

        let search_gh = val.get("searchGh").and_then(|v| v.as_array()).unwrap();
        assert!(!search_gh.is_empty());
        assert_eq!(search_gh[0].get("name").and_then(|n| n.as_str()), Some("mcp__gh__issue_list"));

        let desc_str = val.get("desc").and_then(|v| v.as_str()).unwrap();
        assert!(desc_str.contains("Tool: edit"));
        assert!(desc_str.contains("Edits an existing file"));
        assert!(desc_str.contains("\"path\""));

        assert_eq!(val.get("missing"), None);
    }

    // 8. Verify await describeNamespace("gh") in QuickJS.
    #[tokio::test]
    async fn test_describe_namespace() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(MockNamedTool {
                name: "edit".to_string(),
                description: "Edits a file.".to_string(),
                parameters: serde_json::json!({}),
            }),
            Arc::new(MockNamedTool {
                name: "mcp__gh__issue_list".to_string(),
                description: "Lists issues.".to_string(),
                parameters: serde_json::json!({}),
            }),
            Arc::new(MockNamedTool {
                name: "mcp__gh__issue_view".to_string(),
                description: "Views an issue.".to_string(),
                parameters: serde_json::json!({}),
            }),
        ];

        let tool_defs: Vec<mehh_ai::ToolDefinition> =
            tools.iter().map(|t| tool_to_definition(t.as_ref())).collect();

        let code = r#"
            const gh = await describeNamespace("gh");
            const builtin = await describeNamespace("builtin");
            const unknown = await describeNamespace("unknown_service");
            return { gh, builtin, unknown };
        "#;

        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();
        let result = run_codemode(
            code,
            &tool_defs,
            dummy_executor(),
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;

        assert!(result.ok, "Expected run_codemode to succeed: {:?}", result.error);
        let val = result.value.expect("Expected result value");

        let gh_obj = val.get("gh").expect("Expected gh namespace");
        assert_eq!(gh_obj.get("name").and_then(|n| n.as_str()), Some("gh"));
        let gh_tools = gh_obj.get("tools").and_then(|t| t.as_array()).unwrap();
        assert_eq!(gh_tools.len(), 2);

        let builtin_obj = val.get("builtin").expect("Expected builtin namespace");
        assert_eq!(builtin_obj.get("name").and_then(|n| n.as_str()), Some("builtin"));
        let builtin_tools = builtin_obj.get("tools").and_then(|t| t.as_array()).unwrap();
        assert_eq!(builtin_tools.len(), 1);

        assert_eq!(val.get("unknown"), None);
    }

    // 9. Verify models.list(), models.resolve(), and models.getAvailableOfType() in QuickJS.
    #[tokio::test]
    async fn test_models_global_helper() {
        let code = r#"
            const list = models.list();
            const resolved = models.resolve("claude-sonnet-4-5");
            const gpt = models.resolve("openai", "gpt-4o");
            const googleModels = models.getAvailableOfType("google");
            return {
                listCount: list.length,
                firstId: list[0].id,
                firstContextWindow: list[0].contextWindow,
                resolvedId: resolved ? resolved.id : null,
                resolvedProvider: resolved ? resolved.provider : null,
                gptId: gpt ? gpt.id : null,
                googleCount: googleModels.length,
            };
        "#;

        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig::default();
        let result = run_codemode(
            code,
            &[],
            dummy_executor(),
            kv,
            &config,
            CancellationToken::new(),
        )
        .await;

        assert!(result.ok, "Expected run_codemode to succeed: {:?}", result.error);
        let val = result.value.expect("Expected result value");

        let list_count = val.get("listCount").and_then(|v| v.as_u64()).unwrap();
        assert!(list_count > 0);

        let first_id = val.get("firstId").and_then(|v| v.as_str()).unwrap();
        assert!(!first_id.is_empty());

        let context_window = val.get("firstContextWindow").and_then(|v| v.as_u64()).unwrap();
        assert!(context_window > 0);

        let resolved_id = val.get("resolvedId").and_then(|v| v.as_str()).unwrap();
        assert_eq!(resolved_id, "claude-sonnet-4-5");

        let resolved_provider = val.get("resolvedProvider").and_then(|v| v.as_str()).unwrap();
        assert_eq!(resolved_provider, "anthropic");

        let gpt_id = val.get("gptId").and_then(|v| v.as_str()).unwrap();
        assert_eq!(gpt_id, "gpt-4o");

        let google_count = val.get("googleCount").and_then(|v| v.as_u64()).unwrap();
        assert!(google_count > 0);
    }
}
