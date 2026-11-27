use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use mehh_agent::{tool_to_definition, AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use mehh_ai::{ImageContent, TextContent, ToolCall, ToolResultContentBlock};
use tokio_util::sync::CancellationToken;

use crate::sandbox::{
    decoded_base64_len, resolve_tool_meta, run_codemode_with_meta, to_identifier, CodemodeConfig,
    CodemodeOutputItem, PreparedToolMeta, SandboxToolExecutor, CODEMODE_MAX_IMAGES,
    CODEMODE_MAX_IMAGE_BYTES,
};

struct ToolCache {
    meta: PreparedToolMeta,
    by_name: HashMap<String, usize>,
    by_ident: HashMap<String, usize>,
}

impl ToolCache {
    fn from_tools(tools: &[Arc<dyn AgentTool>]) -> Self {
        let mut defs = Vec::with_capacity(tools.len());
        let mut by_name = HashMap::with_capacity(tools.len());
        let mut by_ident = HashMap::with_capacity(tools.len());
        for (idx, t) in tools.iter().enumerate() {
            defs.push(tool_to_definition(t.as_ref()));
            by_name.entry(t.name().to_string()).or_insert(idx);
            by_ident.entry(to_identifier(t.name())).or_insert(idx);
        }
        let meta = resolve_tool_meta(Arc::from(defs));
        Self {
            meta,
            by_name,
            by_ident,
        }
    }
}

/// Returns the path the inspector approved for this call, if any. The executor
/// binds it into the nested call so path-bearing tools can revalidate at write
/// time and close the inspection-to-execution (TOCTOU) gap.
pub type ToolCallInterceptor = Arc<
    dyn Fn(
            &str,
            &serde_json::Map<String, serde_json::Value>,
        ) -> futures::future::BoxFuture<'static, Result<Option<std::path::PathBuf>, String>>
        + Send
        + Sync,
>;

pub struct CodemodeTool {
    tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
    cache: Arc<RwLock<Arc<ToolCache>>>,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: CodemodeConfig,
    interceptor: Option<ToolCallInterceptor>,
}

impl CodemodeTool {
    pub fn new(tools: Vec<Arc<dyn AgentTool>>) -> Self {
        let filtered_tools: Vec<Arc<dyn AgentTool>> = tools
            .into_iter()
            .filter(|t| t.name() != "codemode")
            .collect();
        let cache = Arc::new(ToolCache::from_tools(&filtered_tools));
        Self {
            tools: Arc::new(RwLock::new(filtered_tools)),
            cache: Arc::new(RwLock::new(cache)),
            kv_store: Arc::new(Mutex::new(HashMap::new())),
            config: CodemodeConfig::default(),
            interceptor: None,
        }
    }

    /// Replaces the active tools exposed inside the sandbox.
    pub fn replace_tools(&self, tools: Vec<Arc<dyn AgentTool>>) {
        let filtered_tools: Vec<Arc<dyn AgentTool>> = tools
            .into_iter()
            .filter(|t| t.name() != "codemode")
            .collect();
        let next_cache = Arc::new(ToolCache::from_tools(&filtered_tools));
        *mehh_ai::write_recover(&self.cache) = next_cache;
        let mut guard = mehh_ai::write_recover(&self.tools);
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

    pub fn with_interceptor(mut self, interceptor: ToolCallInterceptor) -> Self {
        self.interceptor = Some(interceptor);
        self
    }

    pub fn set_interceptor(&mut self, interceptor: Option<ToolCallInterceptor>) {
        self.interceptor = interceptor;
    }

    pub fn kv_store(&self) -> Arc<Mutex<HashMap<String, serde_json::Value>>> {
        self.kv_store.clone()
    }

    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        mehh_ai::read_recover(&self.tools).clone()
    }

    /// Shared handle to the live tool list. Interceptors can read it on every
    /// call so `replace_tools` cannot leave them holding a stale snapshot.
    pub fn tools_handle(&self) -> Arc<RwLock<Vec<Arc<dyn AgentTool>>>> {
        self.tools.clone()
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

        let current_tools: Arc<[Arc<dyn AgentTool>]> =
            Arc::from(mehh_ai::read_recover(&self.tools).as_slice());
        let cache = Arc::clone(&*mehh_ai::read_recover(&self.cache));

        let tools = Arc::clone(&current_tools);
        let tool_call_prefix = tool_call_id.to_string();
        let interceptor = self.interceptor.clone();
        let executor_cache = Arc::clone(&cache);
        let call_seq = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let executor: SandboxToolExecutor = Arc::new(
            move |tool_name: &str, args: serde_json::Value, call_cancel: CancellationToken| {
                let tools = Arc::clone(&tools);
                let name_str = tool_name.to_string();
                let prefix = tool_call_prefix.clone();
                let interceptor = interceptor.clone();
                let cache = Arc::clone(&executor_cache);
                let call_seq = call_seq.clone();
                Box::pin(async move {
                    // Prefer an exact tool name match before sanitized JS identifier fallback
                    // so e.g. `foo-bar` never shadows `foo_bar` when `foo_bar_2` is invoked.
                    let idx = cache
                        .by_name
                        .get(&name_str)
                        .or_else(|| cache.by_ident.get(&name_str))
                        .copied();
                    let idx = match idx {
                        Some(i) => i,
                        None => return Err(format!("Tool '{name_str}' not found")),
                    };
                    let tool = tools[idx].clone();
                    let canonical_name = tool.name();
                    let params_map = match args {
                        serde_json::Value::Object(m) => m,
                        _ => return Err("arguments must be an object".to_string()),
                    };
                    let def = cache
                        .meta
                        .defs
                        .get(idx)
                        .ok_or_else(|| format!("No schema for tool '{canonical_name}'"))?;
                    let seq = call_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let nested_call_id = format!("{prefix}/{canonical_name}#{seq}");
                    let tool_call = ToolCall {
                        id: nested_call_id.clone(),
                        name: canonical_name.to_string(),
                        arguments: params_map,
                        arguments_error: None,
                        thought_signature: None,
                    };
                    let mut validated =
                        mehh_ai::validation::validate_tool_arguments(def, &tool_call)?;
                    validated.remove(mehh_agent::INSPECTED_TARGET_ARG);
                    // Always pass the canonical tool name to the inspector: the
                    // argument may be a deduplicated JavaScript identifier.
                    if let Some(ref intercept) = interceptor {
                        if let Some(target) = intercept(canonical_name, &validated).await? {
                            validated.insert(
                                mehh_agent::INSPECTED_TARGET_ARG.to_string(),
                                serde_json::Value::String(target.to_string_lossy().into_owned()),
                            );
                        }
                    }
                    let tool_res = tool
                        .execute(&nested_call_id, validated, call_cancel, None)
                        .await?;
                    let mut text_parts = Vec::new();
                    let mut images = Vec::new();
                    for c in tool_res.content {
                        match c {
                            ToolResultContentBlock::Text(t) => text_parts.push(t.text),
                            ToolResultContentBlock::Image(img) => {
                                images.push(serde_json::json!({
                                    "type": "image",
                                    "mimeType": img.mime_type,
                                    "data": img.data
                                }));
                            }
                        }
                    }

                    let text = if text_parts.len() == 1 {
                        text_parts.pop().unwrap_or_default()
                    } else {
                        text_parts.join("\n")
                    };

                    if tool_res.is_error.unwrap_or(false) {
                        return Err(format!("Tool '{name_str}' reported an error: {text}"));
                    }
                    // Preserve a plain string for the common case, but never drop
                    // `details`/`structured_content` or tool-produced images.
                    if tool_res.details.is_none()
                        && tool_res.structured_content.is_none()
                        && images.is_empty()
                    {
                        return Ok(serde_json::Value::String(text));
                    }
                    let mut obj = serde_json::Map::new();
                    obj.insert("content".to_string(), serde_json::Value::String(text));
                    if !images.is_empty() {
                        obj.insert("images".to_string(), serde_json::Value::Array(images));
                    }
                    if let Some(details) = tool_res.details {
                        obj.insert("details".to_string(), details);
                    }
                    if let Some(sc) = tool_res.structured_content {
                        obj.insert("structuredContent".to_string(), sc);
                    }
                    Ok(serde_json::Value::Object(obj))
                })
            },
        );

        let res = run_codemode_with_meta(
            code,
            cache.meta.clone(),
            executor,
            self.kv_store.clone(),
            &self.config,
            cancel,
        )
        .await;

        let mut content: Vec<ToolResultContentBlock> = Vec::new();
        let mut text_lines: Vec<String> = Vec::new();
        let mut image_count = 0usize;

        for item in res.items {
            match item {
                CodemodeOutputItem::Text(t) => {
                    text_lines.push(t);
                }
                CodemodeOutputItem::Console { level, text } => {
                    text_lines.push(format!("[{level}] {text}"));
                }
                CodemodeOutputItem::Image { data, mime_type } => {
                    if !matches!(
                        mime_type.as_str(),
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ) {
                        return Err(format!("Unsupported output image type: {mime_type}"));
                    }
                    if decoded_base64_len(&data) > CODEMODE_MAX_IMAGE_BYTES {
                        return Err("Output image exceeds the 2 MiB limit".to_string());
                    }
                    image_count += 1;
                    if image_count > CODEMODE_MAX_IMAGES {
                        return Err("Too many output images (maximum 4)".to_string());
                    }
                    content.push(ToolResultContentBlock::Image(ImageContent {
                        data,
                        mime_type,
                    }));
                }
            }
        }

        if let Some(ref val) = res.value {
            if !val.is_null() {
                let val_str = serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string());
                text_lines.push(format!("Return value:\n{val_str}"));
            }
        }

        if !res.ok {
            if let Some(err) = res.error {
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

    #[tokio::test]
    async fn test_interceptor_approved_path_is_bound_into_nested_tool_args() {
        // A path-bearing nested tool must receive the exact target the inspector
        // approved, so it can revalidate before writing (TOCTOU binding).
        struct SpyTool {
            seen: Arc<Mutex<Option<serde_json::Map<String, serde_json::Value>>>>,
        }

        #[async_trait]
        impl AgentTool for SpyTool {
            fn name(&self) -> &str {
                "write"
            }

            fn description(&self) -> &str {
                "Writes a file."
            }

            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                })
            }

            async fn execute(
                &self,
                _tool_call_id: &str,
                params: serde_json::Map<String, serde_json::Value>,
                _cancel: CancellationToken,
                _on_update: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                *self.seen.lock().unwrap() = Some(params);
                Ok(AgentToolResult::text("ok"))
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let nested = Arc::new(SpyTool { seen: seen.clone() });
        let approved = std::path::PathBuf::from("/approved/target.txt");
        let interceptor: ToolCallInterceptor = Arc::new(move |_name, _args| {
            let approved = approved.clone();
            Box::pin(async move { Ok(Some(approved)) })
        });
        let codemode = CodemodeTool::new(vec![nested]).with_interceptor(interceptor);

        let params = serde_json::json!({
            "code": "await tools.write({ path: 'link.txt' }); return 'done';"
        })
        .as_object()
        .unwrap()
        .clone();
        let res = codemode
            .execute("tc", params, CancellationToken::new(), None)
            .await;
        assert!(res.is_ok(), "codemode failed: {:?}", res.err());

        let seen_args = seen
            .lock()
            .unwrap()
            .clone()
            .expect("nested tool must have been called");
        assert_eq!(
            seen_args
                .get(mehh_agent::INSPECTED_TARGET_ARG)
                .and_then(|v| v.as_str()),
            Some("/approved/target.txt"),
            "approved target must be bound into the nested tool call"
        );
    }

    #[tokio::test]
    async fn test_colliding_sanitized_tool_names_dispatch_to_exact_tool() {
        struct NamedTool {
            tool_name: &'static str,
            calls: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl AgentTool for NamedTool {
            fn name(&self) -> &str {
                self.tool_name
            }
            fn description(&self) -> &str {
                "named test tool"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _tool_call_id: &str,
                _params: serde_json::Map<String, serde_json::Value>,
                _cancel: CancellationToken,
                _on_update: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                mehh_ai::lock_recover(&self.calls).push(self.tool_name.to_string());
                Ok(AgentToolResult::text(self.tool_name))
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let t1 = Arc::new(NamedTool { tool_name: "foo-bar", calls: calls.clone() });
        let t2 = Arc::new(NamedTool { tool_name: "foo_bar", calls: calls.clone() });
        let codemode = CodemodeTool::new(vec![t1, t2]);

        let params = serde_json::json!({
            "code": "const a = await tools.foo_bar({}); const b = await tools.foo_bar_2({}); return [a, b];"
        })
        .as_object()
        .unwrap()
        .clone();
        let res = codemode
            .execute("tc", params, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(res.details, Some(serde_json::json!(["foo-bar", "foo_bar"])));
        assert_eq!(
            *mehh_ai::lock_recover(&calls),
            vec!["foo-bar".to_string(), "foo_bar".to_string()]
        );
    }

    #[tokio::test]
    async fn test_js_forged_inspected_target_arg_is_stripped_before_interceptor_and_tool() {
        struct RecordingTool {
            seen: Arc<Mutex<Vec<serde_json::Map<String, serde_json::Value>>>>,
        }

        #[async_trait]
        impl AgentTool for RecordingTool {
            fn name(&self) -> &str {
                "write"
            }

            fn description(&self) -> &str {
                "Writes a file."
            }

            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                })
            }

            async fn execute(
                &self,
                _tool_call_id: &str,
                params: serde_json::Map<String, serde_json::Value>,
                _cancel: CancellationToken,
                _on_update: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                mehh_ai::lock_recover(&self.seen).push(params);
                Ok(AgentToolResult::text("ok"))
            }
        }

        let tool_seen = Arc::new(Mutex::new(Vec::new()));
        let interceptor_seen = Arc::new(Mutex::new(Vec::new()));
        let interceptor_seen_clone = interceptor_seen.clone();

        let nested = Arc::new(RecordingTool {
            seen: tool_seen.clone(),
        });
        let interceptor: ToolCallInterceptor = Arc::new(move |_name, args| {
            mehh_ai::lock_recover(&interceptor_seen_clone).push(args.clone());
            Box::pin(async move { Ok(None) })
        });
        let codemode = CodemodeTool::new(vec![nested]).with_interceptor(interceptor);

        let js = format!(
            "await tools.write({{ path: 'safe.txt', {}: '/forged/evil.txt' }}); return 'done';",
            serde_json::to_string(mehh_agent::INSPECTED_TARGET_ARG).unwrap()
        );
        let params = serde_json::json!({ "code": js })
            .as_object()
            .unwrap()
            .clone();
        let res = codemode
            .execute("tc", params, CancellationToken::new(), None)
            .await;
        assert!(res.is_ok(), "codemode failed: {:?}", res.err());

        let seen_by_interceptor = mehh_ai::lock_recover(&interceptor_seen);
        assert_eq!(seen_by_interceptor.len(), 1);
        assert!(
            !seen_by_interceptor[0].contains_key(mehh_agent::INSPECTED_TARGET_ARG),
            "forged inspected target arg must be stripped before interceptor"
        );

        let seen_by_tool = mehh_ai::lock_recover(&tool_seen);
        assert_eq!(seen_by_tool.len(), 1);
        assert!(
            !seen_by_tool[0].contains_key(mehh_agent::INSPECTED_TARGET_ARG),
            "forged inspected target arg must be stripped before tool.execute"
        );
    }

    #[tokio::test]
    async fn test_nested_tool_image_includes_type_image_field() {
        struct ImageTool;

        #[async_trait]
        impl AgentTool for ImageTool {
            fn name(&self) -> &str {
                "img_tool"
            }

            fn description(&self) -> &str {
                "Returns an image."
            }

            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }

            async fn execute(
                &self,
                _tool_call_id: &str,
                _params: serde_json::Map<String, serde_json::Value>,
                _cancel: CancellationToken,
                _on_update: Option<AgentToolUpdateCallback>,
            ) -> Result<AgentToolResult, String> {
                Ok(AgentToolResult {
                    content: vec![ToolResultContentBlock::Image(ImageContent {
                        // Valid 1x1 PNG
                        data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".to_string(),
                        mime_type: "image/png".to_string(),
                    })],
                    ..Default::default()
                })
            }
        }

        let codemode = CodemodeTool::new(vec![Arc::new(ImageTool)]);
        let params = serde_json::json!({
            "code": "const r = await tools.img_tool({}); image(r.images[0]); return r.images[0].type;"
        })
        .as_object()
        .unwrap()
        .clone();

        let res = codemode
            .execute("tc_img", params, CancellationToken::new(), None)
            .await
            .expect("image object from nested tool must be accepted by image()");
        assert_eq!(res.details, Some(serde_json::json!("image")));
        assert!(
            res.content
                .iter()
                .any(|b| matches!(b, ToolResultContentBlock::Image(_))),
            "expected image content block in codemode result"
        );
    }
}
