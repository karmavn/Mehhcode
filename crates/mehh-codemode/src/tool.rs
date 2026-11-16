use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use mehh_agent::{tool_to_definition, AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, TextContent, ToolDefinition, ToolResultContentBlock};
use tokio_util::sync::CancellationToken;

use crate::sandbox::{
    run_codemode, to_identifier, CodemodeConfig, CodemodeOutputItem, SandboxToolExecutor,
};

pub struct CodemodeTool {
    tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: CodemodeConfig,
}

impl CodemodeTool {
    pub fn new(tools: Vec<Arc<dyn AgentTool>>) -> Self {
        let filtered_tools = tools
            .into_iter()
            .filter(|t| t.name() != "codemode")
            .collect();
        Self {
            tools: Arc::new(RwLock::new(filtered_tools)),
            kv_store: Arc::new(Mutex::new(HashMap::new())),
            config: CodemodeConfig::default(),
        }
    }

    /// Replaces the active tools exposed inside the sandbox.
    pub fn replace_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        let filtered_tools = tools
            .into_iter()
            .filter(|t| t.name() != "codemode")
            .collect();
        let mut guard = self.tools.write().unwrap();
        *guard = filtered_tools;
    }

    pub fn with_config(mut self, config: CodemodeConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_kv_store(
        mut self,
        kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    ) -> Self {
        self.kv_store = kv_store;
        self
    }

    pub fn kv_store(&self) -> Arc<Mutex<HashMap<String, serde_json::Value>>> {
        self.kv_store.clone()
    }

    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.tools.read().unwrap().clone()
    }
}

const CODEMODE_MAX_OUTPUT_BYTES: usize = 40_000;

fn truncate_codemode_output(text: &str) -> String {
    if text.len() <= CODEMODE_MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut head_end = 20_000.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(19_000).max(head_end);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = text[head_end..tail_start].chars().count();
    let head = &text[..head_end];
    let tail = &text[tail_start..];
    format!("{head}\n\n[... {omitted} characters truncated ...]\n\n{tail}")
}

#[async_trait]
impl AgentTool for CodemodeTool {
    fn name(&self) -> &str {
        "codemode"
    }

    fn description(&self) -> &str {
        "Executes JavaScript in an isolated QuickJS sandbox.\n\
         - Invoke tools: `await tools.<name>(args)` (e.g. `await tools.read({ path: \"Cargo.toml\" })`)\n\
         - Discover tools: `await searchTools(query, options?)`, `await describeTool(name)`, `await describeNamespace(name)`\n\
         - Models catalog: `models.list()`, `models.resolve(providerOrId, modelId?)`, `models.getAvailableOfType(provider?)`\n\
         - List all tools: `ALL_TOOLS` contains [{ name, description }, ...]\n\
         - Output: `console.log(...)`, `text(value)`, `image(dataUrlOrObject)`\n\
         - Persistent storage: `store(key, value)` and `load(key)` persist across scripts\n\
         - Completion: `return <value>` returns the final result; `exit()` exits immediately"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript source code to evaluate in the sandbox."
                }
            },
            "required": ["code"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Map<String, serde_json::Value>,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        let code = params
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: code".to_string())?;

        let current_tools = self.tools.read().unwrap().clone();
        let tool_defs: Vec<ToolDefinition> = current_tools
            .iter()
            .map(|t| tool_to_definition(t.as_ref()))
            .collect();

        let tools = current_tools;
        let tool_call_prefix = tool_call_id.to_string();
        let cancel_executor = cancel.clone();

        let executor: SandboxToolExecutor = Arc::new(move |tool_name: &str, args: serde_json::Value| {
            let tools = tools.clone();
            let name_str = tool_name.to_string();
            let prefix = tool_call_prefix.clone();
            let cancel = cancel_executor.clone();

            Box::pin(async move {
                let matching = tools.iter().find(|t| {
                    t.name() == name_str || to_identifier(t.name()) == name_str
                });
                let tool = match matching {
                    Some(t) => t.clone(),
                    None => return Err(format!("Tool '{name_str}' not found")),
                };

                let params_map = match args {
                    serde_json::Value::Object(m) => m,
                    _ => serde_json::Map::new(),
                };

                let nested_call_id = format!("{prefix}/{name_str}");
                let tool_res = tool.execute(&nested_call_id, params_map, cancel, None).await?;

                if let Some(details) = tool_res.details {
                    return Ok(details);
                }

                let text = tool_res
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                Ok(serde_json::Value::String(text))
            })
        });

        let res = run_codemode(
            code,
            &tool_defs,
            executor,
            self.kv_store.clone(),
            &self.config,
            cancel,
        )
        .await;

        let mut content: Vec<ToolResultContentBlock> = Vec::new();
        let mut text_lines: Vec<String> = Vec::new();

        for item in &res.items {
            match item {
                CodemodeOutputItem::Text(t) => {
                    text_lines.push(t.clone());
                }
                CodemodeOutputItem::Console { level, text } => {
                    text_lines.push(format!("[{level}] {text}"));
                }
                CodemodeOutputItem::Image { data, mime_type } => {
                    content.push(ToolResultContentBlock::Image(ImageContent {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    }));
                }
            }
        }

        if let Some(val) = &res.value {
            if !val.is_null() {
                let val_str = serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string());
                text_lines.push(format!("Return value:\n{val_str}"));
            }
        }

        if !res.ok {
            if let Some(err) = &res.error {
                text_lines.push(format!("Script error: {err}"));
            } else {
                text_lines.push("Script failed with unknown error".to_string());
            }
        }

        let mut combined_text = text_lines.join("\n\n");
        if combined_text.is_empty() && content.is_empty() {
            combined_text = if res.ok {
                "Script completed with no output.".to_string()
            } else {
                "Script failed with no output.".to_string()
            };
        }

        combined_text = truncate_codemode_output(&combined_text);

        if !res.ok {
            return Err(combined_text);
        }

        if !combined_text.is_empty() {
            content.insert(
                0,
                ToolResultContentBlock::Text(TextContent {
                    text: combined_text,
                    text_signature: None,
                }),
            );
        }

        Ok(AgentToolResult {
            content,
            details: res.value,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_codemode_output_non_ascii_safe() {
        // 3-byte UTF-8 chars across 20,000 and len - 19,000 boundaries
        let text = "中".repeat(16_000); // 48,000 bytes
        let truncated = truncate_codemode_output(&text);
        assert!(truncated.contains("characters truncated"));
    }

    #[tokio::test]
    async fn test_codemode_failed_script_returns_err_and_metadata() {
        let tool = CodemodeTool::new(Vec::new());
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Sequential);
        assert!(tool.description().contains("tools.read("));
        assert!(!tool.description().contains("read_file"));

        let params = serde_json::json!({ "code": "throw new Error('boom');" })
            .as_object()
            .unwrap()
            .clone();
        let err = tool
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap_err();
        assert!(err.contains("boom"));
    }
}
