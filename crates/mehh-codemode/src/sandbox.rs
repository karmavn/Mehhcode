use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use mehh_ai::ToolDefinition;
use rquickjs::FromJs;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct CodemodeConfig {
    pub memory_limit_bytes: usize,
    pub timeout: Duration,
    pub max_output_chars: usize,
    pub max_output_items: usize,
}

impl Default for CodemodeConfig {
    fn default() -> Self {
        Self {
            memory_limit_bytes: 256 * 1024 * 1024,
            timeout: Duration::from_secs(30),
            max_output_chars: 16 * 1024 * 1024,
            max_output_items: 100_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CodemodeOutputItem {
    Text(String),
    Console { level: String, text: String },
    Image { data: String, mime_type: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodeResult {
    pub ok: bool,
    pub value: Option<serde_json::Value>,
    pub error: Option<String>,
    pub items: Vec<CodemodeOutputItem>,
}

pub type SandboxToolExecutor = Arc<
    dyn Fn(&str, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value, String>>
        + Send
        + Sync,
>;

#[derive(Serialize)]
struct ToolMeta {
    name: String,
    js_name: String,
    description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSummary {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub api: String,
    pub context_window: u64,
    pub reasoning: bool,
}

impl From<mehh_ai::Model> for ModelSummary {
    fn from(m: mehh_ai::Model) -> Self {
        Self {
            id: m.id,
            name: m.name,
            provider: m.provider,
            api: m.api,
            context_window: m.context_window,
            reasoning: m.reasoning,
        }
    }
}

pub fn to_identifier(name: &str) -> String {
    let mut result = String::new();
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            if i == 0 && ch.is_ascii_digit() {
                result.push('_');
            }
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        "_".to_string()
    } else {
        result
    }
}

const PRELUDE_JS: &str = r#"function(__TOOLS_META__) {
    const toolMap = new Map();
    const allTools = [];
    for (const t of __TOOLS_META__) {
        toolMap.set(t.name, t.name);
        toolMap.set(t.js_name, t.name);
        allTools.push(Object.freeze({ name: t.js_name, description: t.description }));
    }
    const toolsProxy = new Proxy({}, {
        get(target, prop) {
            if (typeof prop !== "string" || prop === "then" || prop === "toJSON") {
                return undefined;
            }
            if (toolMap.has(prop)) {
                const toolName = toolMap.get(prop);
                return async function(args) {
                    const argsJson = JSON.stringify(args !== undefined ? args : {});
                    const resJson = __host_call_tool(toolName, argsJson);
                    let res;
                    try {
                        res = JSON.parse(resJson);
                    } catch (e) {
                        throw new Error("Failed to parse tool call response: " + e.message);
                    }
                    if (!res.ok) {
                        throw new Error(res.error || "Tool call failed");
                    }
                    return res.value;
                };
            }
            let msg = "tools." + prop + " does not exist.";
            const available = Array.from(toolMap.keys());
            if (available.length > 0) {
                msg += " Available tools: " + available.join(", ") + ".";
            }
            msg += " ALL_TOOLS lists every available tool.";
            throw new TypeError(msg);
        },
        has(target, prop) {
            return toolMap.has(prop);
        },
        ownKeys(target) {
            return Array.from(toolMap.keys());
        },
        getOwnPropertyDescriptor(target, prop) {
            if (toolMap.has(prop)) {
                return { configurable: true, enumerable: true, writable: false, value: this.get(target, prop) };
            }
            return undefined;
        }
    });

    function outputText(v) {
        if (v === undefined || v === null || (typeof v !== "object" && typeof v !== "function")) {
            return String(v);
        }
        try {
            const json = JSON.stringify(v);
            return json === undefined ? String(v) : json;
        } catch {
            return String(v);
        }
    }

    const consoleObj = {};
    for (const level of ["log", "info", "warn", "error", "debug"]) {
        consoleObj[level] = (...args) => {
            const formatted = args.map(outputText).join(" ");
            const err = __host_log(level, formatted);
            if (err) throw new RangeError(err);
        };
    }
    Object.freeze(consoleObj);

    function text(v) {
        const s = outputText(v);
        const err = __host_text(s);
        if (err) throw new RangeError(err);
    }

    const IMAGE_SIGNATURES = [
        ["image/png", /^iVBORw0KGg/],
        ["image/jpeg", /^\/9j\//],
        ["image/gif", /^R0lGOD/],
        ["image/webp", /^UklG/],
    ];

    function image(value) {
        let url = "";
        if (typeof value === "string") {
            url = value;
        } else if (value && typeof value === "object") {
            if (typeof value.image_url === "string") {
                url = value.image_url;
            } else if (value.type === "image" && typeof value.data === "string") {
                if (value.data.toLowerCase().startsWith("data:")) {
                    url = value.data;
                } else {
                    const mime = value.mimeType || value.mime_type || "image/png";
                    url = "data:" + mime + ";base64," + value.data;
                }
            }
        }
        if (!url) {
            throw new TypeError("image expects an image URL string, { image_url }, or { type: 'image', data, mimeType }");
        }
        const colon = url.indexOf(":");
        const scheme = colon === -1 ? "" : url.slice(0, colon).toLowerCase();
        if (scheme === "http" || scheme === "https") {
            throw new TypeError("remote image URLs are not supported in tool outputs. Pass a base64 data URI instead");
        }
        const comma = url.indexOf(",");
        if (scheme !== "data" || comma === -1) {
            throw new TypeError("invalid image output. Pass a base64 data URI instead");
        }
        const header = url.slice(colon + 1, comma).split(";");
        if (!header.slice(1).some(p => p.toLowerCase() === "base64")) {
            throw new TypeError("invalid image output. Pass a base64 data URI instead");
        }
        const data = url.slice(comma + 1).replace(/\s+/g, "");
        if (data.length % 4 !== 0 || !/^[A-Za-z0-9+/]+={0,2}$/.test(data)) {
            throw new TypeError("invalid image output. The image data is not valid base64");
        }
        const head = data.slice(0, 16);
        let detectedMime = null;
        for (const [mime, pattern] of IMAGE_SIGNATURES) {
            if (pattern.test(head)) {
                detectedMime = mime;
                break;
            }
        }
        if (!detectedMime) {
            throw new TypeError("invalid image output. The image data is not a PNG, JPEG, GIF, or WebP image");
        }
        const err = __host_image(data, detectedMime);
        if (err) throw new RangeError(err);
    }

    const MAX_STORE_VAL = 256 * 1024;
    function store(key, value) {
        if (typeof key !== "string") {
            throw new TypeError("store() key must be a string");
        }
        if (value === undefined) {
            const err = __host_store(key, "__UNDEFINED__");
            if (err) throw new RangeError(err);
            return;
        }
        let json;
        try {
            json = JSON.stringify(value);
        } catch (e) {
            throw new TypeError("store('" + key + "') value is not JSON-serializable: " + e.message);
        }
        if (json === undefined) {
            throw new TypeError("store('" + key + "') value is not JSON-serializable");
        }
        if (json.length > MAX_STORE_VAL) {
            throw new RangeError("store('" + key + "') value exceeds 256KB limit");
        }
        const err = __host_store(key, json);
        if (err) throw new RangeError(err);
    }

    function load(key) {
        if (typeof key !== "string") {
            throw new TypeError("load() key must be a string");
        }
        const raw = __host_load(key);
        if (raw === "" || raw === "__UNDEFINED__") {
            return undefined;
        }
        try {
            return JSON.parse(raw);
        } catch {
            return undefined;
        }
    }

    function exit() {
        __host_exit();
        throw new Error("__EXIT__");
    }

    async function searchTools(query, options) {
        if (typeof query !== "string") {
            throw new TypeError("searchTools(query) requires query to be a string");
        }
        const optJson = JSON.stringify(options !== undefined ? options : {});
        const resJson = __host_search_tools(query, optJson);
        return JSON.parse(resJson);
    }

    async function describeTool(name) {
        if (typeof name !== "string") {
            throw new TypeError("describeTool(name) requires name to be a string");
        }
        const raw = __host_describe_tool(name);
        if (!raw) {
            return undefined;
        }
        return raw;
    }

    async function describeNamespace(name) {
        if (typeof name !== "string") {
            throw new TypeError("describeNamespace(name) requires name to be a string");
        }
        const raw = __host_describe_namespace(name);
        if (!raw) {
            return undefined;
        }
        return JSON.parse(raw);
    }

    const modelsObj = {
        list() {
            return JSON.parse(__host_models_list());
        },
        resolve(providerOrId, modelId) {
            const mIdJson = JSON.stringify(modelId !== undefined ? modelId : null);
            const res = __host_models_resolve(String(providerOrId), mIdJson);
            if (!res) {
                return undefined;
            }
            return JSON.parse(res);
        },
        getAvailableOfType(provider) {
            const pJson = JSON.stringify(provider !== undefined ? provider : null);
            return JSON.parse(__host_models_get_available(pJson));
        }
    };
    Object.freeze(modelsObj);

    globalThis.tools = toolsProxy;
    globalThis.ALL_TOOLS = Object.freeze(allTools);
    globalThis.console = consoleObj;
    globalThis.text = text;
    globalThis.image = image;
    globalThis.store = store;
    globalThis.load = load;
    globalThis.exit = exit;
    globalThis.searchTools = searchTools;
    globalThis.describeTool = describeTool;
    globalThis.describeNamespace = describeNamespace;
    globalThis.models = modelsObj;
}"#;

fn format_js_error<'js>(
    ctx: &rquickjs::Ctx<'js>,
    exc: rquickjs::Value<'js>,
    fallback: rquickjs::Error,
) -> String {
    if let Some(obj) = exc.as_object() {
        let name: Option<String> = obj.get("name").ok();
        let message: Option<String> = obj.get("message").ok();
        let stack: Option<String> = obj.get("stack").ok();

        if let Some(msg) = message {
            let n = name.unwrap_or_else(|| "Error".to_string());
            if let Some(st) = stack {
                let filtered: Vec<&str> = st
                    .lines()
                    .filter(|l| !l.is_empty())
                    .collect();
                if !filtered.is_empty() {
                    return format!("{n}: {msg}\n{}", filtered.join("\n"));
                }
            }
            return format!("{n}: {msg}");
        }
    }
    if let Ok(Some(s)) = ctx.json_stringify(&exc) {
        if let Ok(str_val) = s.to_string() {
            if str_val != "null" && str_val != "{}" {
                return str_val;
            }
        }
    }
    if let Ok(coerced) = rquickjs::Coerced::<String>::from_js(ctx, exc) {
        if !coerced.0.is_empty() && coerced.0 != "[object Object]" {
            return coerced.0;
        }
    }
    fallback.to_string()
}

pub async fn run_codemode(
    code: &str,
    tool_defs: &[ToolDefinition],
    executor: SandboxToolExecutor,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: &CodemodeConfig,
    cancel: CancellationToken,
) -> CodemodeResult {
    let code = code.to_string();
    let tool_defs = tool_defs.to_vec();
    let config = config.clone();
    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || {
        run_codemode_blocking(
            &code,
            &tool_defs,
            executor,
            kv_store,
            &config,
            cancel,
            handle,
        )
    })
    .await
    .unwrap_or_else(|e| CodemodeResult {
        ok: false,
        value: None,
        error: Some(format!("Codemode task panicked: {e}")),
        items: Vec::new(),
    })
}

fn run_codemode_blocking(
    code: &str,
    tool_defs: &[ToolDefinition],
    executor: SandboxToolExecutor,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: &CodemodeConfig,
    cancel: CancellationToken,
    handle: tokio::runtime::Handle,
) -> CodemodeResult {
    let rt = match rquickjs::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to create QuickJS runtime: {e}")),
                items: Vec::new(),
            };
        }
    };

    rt.set_memory_limit(config.memory_limit_bytes);

    let deadline = Instant::now() + config.timeout;
    let cancel_check = cancel.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        Instant::now() > deadline || cancel_check.is_cancelled()
    })));

    let context = match rquickjs::Context::full(&rt) {
        Ok(c) => c,
        Err(e) => {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to create QuickJS context: {e}")),
                items: Vec::new(),
            };
        }
    };

    let items = Arc::new(Mutex::new(Vec::new()));
    let total_chars = Arc::new(AtomicUsize::new(0));
    let total_items = Arc::new(AtomicUsize::new(0));
    let exited = Arc::new(AtomicBool::new(false));

    context.with(|ctx| {
        let globals = ctx.globals();

        // 1. __host_call_tool
        let handle_tool = handle.clone();
        let executor_tool = executor.clone();
        let host_call_tool = match rquickjs::Function::new(
            ctx.clone(),
            move |name: String, args_json: String| -> String {
                let args_val = serde_json::from_str(&args_json)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let res = handle_tool.block_on((executor_tool)(&name, args_val));
                match res {
                    Ok(v) => serde_json::json!({ "ok": true, "value": v }).to_string(),
                    Err(e) => serde_json::json!({ "ok": false, "error": e }).to_string(),
                }
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_call_tool: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_call_tool", host_call_tool) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_call_tool: {e}")),
                items: Vec::new(),
            };
        }

        // 2. __host_log
        let items_log = items.clone();
        let total_chars_log = total_chars.clone();
        let total_items_log = total_items.clone();
        let max_chars = config.max_output_chars;
        let max_items = config.max_output_items;
        let host_log = match rquickjs::Function::new(
            ctx.clone(),
            move |level: String, text: String| -> String {
                let new_chars = total_chars_log.load(Ordering::Relaxed) + text.len();
                let new_items = total_items_log.load(Ordering::Relaxed) + 1;
                if new_chars > max_chars || new_items > max_items {
                    return "script output exceeded output limit".to_string();
                }
                total_chars_log.store(new_chars, Ordering::Relaxed);
                total_items_log.store(new_items, Ordering::Relaxed);
                items_log
                    .lock()
                    .unwrap()
                    .push(CodemodeOutputItem::Console { level, text });
                String::new()
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_log: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_log", host_log) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_log: {e}")),
                items: Vec::new(),
            };
        }

        // 3. __host_text
        let items_text = items.clone();
        let total_chars_text = total_chars.clone();
        let total_items_text = total_items.clone();
        let host_text = match rquickjs::Function::new(
            ctx.clone(),
            move |text: String| -> String {
                let new_chars = total_chars_text.load(Ordering::Relaxed) + text.len();
                let new_items = total_items_text.load(Ordering::Relaxed) + 1;
                if new_chars > max_chars || new_items > max_items {
                    return "script output exceeded output limit".to_string();
                }
                total_chars_text.store(new_chars, Ordering::Relaxed);
                total_items_text.store(new_items, Ordering::Relaxed);
                items_text
                    .lock()
                    .unwrap()
                    .push(CodemodeOutputItem::Text(text));
                String::new()
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_text: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_text", host_text) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_text: {e}")),
                items: Vec::new(),
            };
        }

        // 4. __host_image
        let items_image = items.clone();
        let total_chars_image = total_chars.clone();
        let total_items_image = total_items.clone();
        let host_image = match rquickjs::Function::new(
            ctx.clone(),
            move |data: String, mime: String| -> String {
                let new_chars = total_chars_image.load(Ordering::Relaxed) + data.len();
                let new_items = total_items_image.load(Ordering::Relaxed) + 1;
                if new_chars > max_chars || new_items > max_items {
                    return "script output exceeded output limit".to_string();
                }
                total_chars_image.store(new_chars, Ordering::Relaxed);
                total_items_image.store(new_items, Ordering::Relaxed);
                items_image.lock().unwrap().push(CodemodeOutputItem::Image {
                    data,
                    mime_type: mime,
                });
                String::new()
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_image: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_image", host_image) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_image: {e}")),
                items: Vec::new(),
            };
        }

        // 5. __host_store
        let kv_store_set = kv_store.clone();
        let host_store = match rquickjs::Function::new(
            ctx.clone(),
            move |key: String, val_json: String| -> String {
                let mut map = kv_store_set.lock().unwrap();
                if val_json == "__UNDEFINED__" {
                    map.remove(&key);
                    return String::new();
                }
                if val_json.len() > 256 * 1024 {
                    return format!("store('{key}') value exceeds 256KB limit");
                }
                let val: serde_json::Value = match serde_json::from_str(&val_json) {
                    Ok(v) => v,
                    Err(e) => return format!("store('{key}') invalid JSON: {e}"),
                };
                let current_total: usize = map
                    .iter()
                    .filter(|(k, _)| k.as_str() != key.as_str())
                    .map(|(k, v)| {
                        k.len()
                            + serde_json::to_string(v)
                                .map(|s| s.len())
                                .unwrap_or(0)
                    })
                    .sum();
                let new_total = current_total + key.len() + val_json.len();
                if new_total > 1024 * 1024 {
                    return "store is full: stored values would exceed 1MB limit".to_string();
                }
                map.insert(key, val);
                String::new()
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_store: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_store", host_store) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_store: {e}")),
                items: Vec::new(),
            };
        }

        // 6. __host_load
        let kv_store_get = kv_store.clone();
        let host_load = match rquickjs::Function::new(
            ctx.clone(),
            move |key: String| -> String {
                let map = kv_store_get.lock().unwrap();
                match map.get(&key) {
                    Some(v) => {
                        serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
                    }
                    None => "__UNDEFINED__".to_string(),
                }
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_load: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_load", host_load) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_load: {e}")),
                items: Vec::new(),
            };
        }

        // 7. __host_exit
        let exited_fn = exited.clone();
        let host_exit = match rquickjs::Function::new(
            ctx.clone(),
            move || -> () {
                exited_fn.store(true, Ordering::SeqCst);
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_exit: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_exit", host_exit) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_exit: {e}")),
                items: Vec::new(),
            };
        }

        // 8. __host_search_tools
        let tool_defs_search = Arc::new(tool_defs.to_vec());
        let host_search_tools = match rquickjs::Function::new(
            ctx.clone(),
            move |query: String, options_json: String| -> String {
                #[derive(Deserialize, Default)]
                struct SearchOptions {
                    limit: Option<usize>,
                    namespace: Option<String>,
                }
                let opts: SearchOptions = serde_json::from_str(&options_json).unwrap_or_default();
                let limit = opts.limit.unwrap_or(8);
                let query_lower = query.to_lowercase();
                let query_tokens: Vec<&str> = query_lower
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|s| !s.is_empty())
                    .collect();

                let mut scored: Vec<(f64, &ToolDefinition)> = Vec::new();

                for tool in tool_defs_search.as_slice() {
                    if let Some(ref ns) = opts.namespace {
                        if ns == "builtin" {
                            if tool.name.starts_with("mcp__") {
                                continue;
                            }
                        } else {
                            let ns_prefix = format!("mcp__{ns}__");
                            let ns_sanitized = ns.replace('-', "_");
                            let ns_prefix_sanitized = format!("mcp__{ns_sanitized}__");
                            let exact = tool.name == *ns;
                            if !exact
                                && !tool.name.starts_with(&ns_prefix)
                                && !tool.name.starts_with(&ns_prefix_sanitized)
                                && !tool.name.starts_with(ns)
                            {
                                continue;
                            }
                        }
                    }

                    let name_lower = tool.name.to_lowercase();
                    let desc_lower = tool.description.to_lowercase();

                    let mut score = 0.0;
                    if !query_lower.is_empty() {
                        if name_lower == query_lower {
                            score += 100.0;
                        } else if name_lower.contains(&query_lower) {
                            score += 50.0;
                        }

                        if desc_lower.contains(&query_lower) {
                            score += 25.0;
                        }

                        for token in &query_tokens {
                            if name_lower.contains(token) {
                                score += 20.0;
                            }
                            let count = desc_lower.matches(token).count();
                            if count > 0 {
                                score += 5.0 * (count as f64).min(5.0);
                            }
                        }
                    } else {
                        score = 1.0;
                    }

                    if score > 0.0 || query_lower.is_empty() {
                        scored.push((score, tool));
                    }
                }

                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

                let results: Vec<serde_json::Value> = scored
                    .into_iter()
                    .take(limit)
                    .map(|(_, t)| {
                        serde_json::json!({
                            "name": to_identifier(&t.name),
                            "description": t.description
                        })
                    })
                    .collect();

                serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string())
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_search_tools: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_search_tools", host_search_tools) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_search_tools: {e}")),
                items: Vec::new(),
            };
        }

        // 9. __host_describe_tool
        let tool_defs_desc = Arc::new(tool_defs.to_vec());
        let host_describe_tool = match rquickjs::Function::new(
            ctx.clone(),
            move |name: String| -> String {
                let found = tool_defs_desc
                    .iter()
                    .find(|t| t.name == name || to_identifier(&t.name) == name);
                match found {
                    Some(t) => {
                        let params_pretty =
                            serde_json::to_string_pretty(&t.parameters).unwrap_or_default();
                        format!(
                            "Tool: {}\nDescription: {}\nParameters:\n{}",
                            t.name, t.description, params_pretty
                        )
                    }
                    None => String::new(),
                }
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_describe_tool: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_describe_tool", host_describe_tool) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_describe_tool: {e}")),
                items: Vec::new(),
            };
        }

        // 10. __host_describe_namespace
        let tool_defs_ns = Arc::new(tool_defs.to_vec());
        let host_describe_namespace = match rquickjs::Function::new(
            ctx.clone(),
            move |name: String| -> String {
                let tools: Vec<String> = if name == "builtin" {
                    tool_defs_ns
                        .iter()
                        .filter(|t| !t.name.starts_with("mcp__"))
                        .map(|t| to_identifier(&t.name))
                        .collect()
                } else {
                    let prefix = format!("mcp__{name}__");
                    let sanitized = name.replace('-', "_");
                    let prefix_sanitized = format!("mcp__{sanitized}__");
                    tool_defs_ns
                        .iter()
                        .filter(|t| {
                            t.name.starts_with(&prefix)
                                || t.name.starts_with(&prefix_sanitized)
                                || t.name == name
                        })
                        .map(|t| to_identifier(&t.name))
                        .collect()
                };

                if tools.is_empty() && name != "builtin" {
                    String::new()
                } else {
                    serde_json::json!({
                        "name": name,
                        "tools": tools
                    })
                    .to_string()
                }
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_describe_namespace: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_describe_namespace", host_describe_namespace) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_describe_namespace: {e}")),
                items: Vec::new(),
            };
        }

        // 11. __host_models_list
        let host_models_list = match rquickjs::Function::new(
            ctx.clone(),
            move || -> String {
                let models: Vec<ModelSummary> = mehh_ai::catalog::builtin_models()
                    .into_iter()
                    .map(ModelSummary::from)
                    .collect();
                serde_json::to_string(&models).unwrap_or_else(|_| "[]".to_string())
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_models_list: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_models_list", host_models_list) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_models_list: {e}")),
                items: Vec::new(),
            };
        }

        // 12. __host_models_resolve
        let host_models_resolve = match rquickjs::Function::new(
            ctx.clone(),
            move |provider_or_id: String, model_id_json: String| -> String {
                let model_id: Option<String> = serde_json::from_str(&model_id_json).unwrap_or(None);
                match mehh_ai::catalog::resolve_model(&provider_or_id, model_id.as_deref()) {
                    Some(m) => {
                        let summary = ModelSummary::from(m);
                        serde_json::to_string(&summary).unwrap_or_default()
                    }
                    None => String::new(),
                }
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_models_resolve: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_models_resolve", host_models_resolve) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_models_resolve: {e}")),
                items: Vec::new(),
            };
        }

        // 13. __host_models_get_available
        let host_models_get_available = match rquickjs::Function::new(
            ctx.clone(),
            move |provider_json: String| -> String {
                let provider_filter: Option<String> =
                    serde_json::from_str(&provider_json).unwrap_or(None);
                let all = mehh_ai::catalog::builtin_models();
                let filtered: Vec<ModelSummary> = all
                    .into_iter()
                    .filter(|m| {
                        if let Some(ref p) = provider_filter {
                            m.provider.eq_ignore_ascii_case(p) || m.api.eq_ignore_ascii_case(p)
                        } else {
                            true
                        }
                    })
                    .map(ModelSummary::from)
                    .collect();
                serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".to_string())
            },
        ) {
            Ok(f) => f,
            Err(e) => {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Failed to register __host_models_get_available: {e}")),
                    items: Vec::new(),
                };
            }
        };
        if let Err(e) = globals.set("__host_models_get_available", host_models_get_available) {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Failed to bind __host_models_get_available: {e}")),
                items: Vec::new(),
            };
        }

        // Setup prelude
        let tools_meta: Vec<ToolMeta> = tool_defs
            .iter()
            .map(|t| ToolMeta {
                name: t.name.clone(),
                js_name: to_identifier(&t.name),
                description: t.description.clone(),
            })
            .collect();
        let tools_meta_json =
            serde_json::to_string(&tools_meta).unwrap_or_else(|_| "[]".to_string());
        let prelude_code = format!("(({PRELUDE_JS}))({tools_meta_json});");
        if let Err(e) = ctx.eval::<(), _>(prelude_code.as_str()) {
            let exc = ctx.catch();
            let err_msg = format_js_error(&ctx, exc, e);
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(format!("Prelude error: {err_msg}")),
                items: Vec::new(),
            };
        }

        // Evaluate user code
        let wrapped_code = format!("(async () => {{\n{code}\n}})()");
        let eval_val: rquickjs::Value = match ctx.eval(wrapped_code.as_str()) {
            Ok(v) => v,
            Err(err) => {
                if Instant::now() > deadline {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution timed out".to_string()),
                        items: std::mem::take(&mut *items.lock().unwrap()),
                    };
                }
                if cancel.is_cancelled() {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution was cancelled".to_string()),
                        items: std::mem::take(&mut *items.lock().unwrap()),
                    };
                }
                if exited.load(Ordering::SeqCst) {
                    return CodemodeResult {
                        ok: true,
                        value: None,
                        error: None,
                        items: std::mem::take(&mut *items.lock().unwrap()),
                    };
                }
                let exc = ctx.catch();
                let err_msg = format_js_error(&ctx, exc, err);
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(err_msg),
                    items: std::mem::take(&mut *items.lock().unwrap()),
                };
            }
        };

        if let Some(promise) = eval_val.as_promise() {
            while promise.state() == rquickjs::promise::PromiseState::Pending {
                if !ctx.execute_pending_job() {
                    break;
                }
                if Instant::now() > deadline || cancel.is_cancelled() {
                    break;
                }
            }

            if Instant::now() > deadline {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some("Execution timed out".to_string()),
                    items: std::mem::take(&mut *items.lock().unwrap()),
                };
            }
            if cancel.is_cancelled() {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some("Execution was cancelled".to_string()),
                    items: std::mem::take(&mut *items.lock().unwrap()),
                };
            }
            if exited.load(Ordering::SeqCst) {
                return CodemodeResult {
                    ok: true,
                    value: None,
                    error: None,
                    items: std::mem::take(&mut *items.lock().unwrap()),
                };
            }

            match promise.state() {
                rquickjs::promise::PromiseState::Resolved => {
                    let res: Option<rquickjs::Result<rquickjs::Value>> = promise.result();
                    let val = match res {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => {
                            let exc = ctx.catch();
                            let err_msg = format_js_error(&ctx, exc, e);
                            return CodemodeResult {
                                ok: false,
                                value: None,
                                error: Some(err_msg),
                                items: std::mem::take(&mut *items.lock().unwrap()),
                            };
                        }
                        None => rquickjs::Value::new_undefined(ctx.clone()),
                    };

                    let parsed_value = if val.is_undefined() {
                        None
                    } else {
                        match ctx.json_stringify(&val) {
                            Ok(Some(s)) => {
                                let json_str = s.to_string().unwrap_or_default();
                                serde_json::from_str::<serde_json::Value>(&json_str).ok()
                            }
                            Ok(None) => None,
                            Err(_) => {
                                return CodemodeResult {
                                    ok: false,
                                    value: None,
                                    error: Some("Return value is not JSON-serializable".to_string()),
                                    items: std::mem::take(&mut *items.lock().unwrap()),
                                };
                            }
                        }
                    };

                    CodemodeResult {
                        ok: true,
                        value: parsed_value,
                        error: None,
                        items: std::mem::take(&mut *items.lock().unwrap()),
                    }
                }
                rquickjs::promise::PromiseState::Rejected => {
                    let _ = promise.result::<rquickjs::Value>();
                    let exc = ctx.catch();
                    if exited.load(Ordering::SeqCst) {
                        return CodemodeResult {
                            ok: true,
                            value: None,
                            error: None,
                            items: std::mem::take(&mut *items.lock().unwrap()),
                        };
                    }
                    let err_msg = format_js_error(&ctx, exc, rquickjs::Error::Exception);
                    CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some(err_msg),
                        items: std::mem::take(&mut *items.lock().unwrap()),
                    }
                }
                rquickjs::promise::PromiseState::Pending => CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(
                        "The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here."
                            .to_string(),
                    ),
                    items: std::mem::take(&mut *items.lock().unwrap()),
                },
            }
        } else {
            let parsed_value = if eval_val.is_undefined() {
                None
            } else {
                match ctx.json_stringify(&eval_val) {
                    Ok(Some(s)) => {
                        let json_str = s.to_string().unwrap_or_default();
                        serde_json::from_str::<serde_json::Value>(&json_str).ok()
                    }
                    Ok(None) => None,
                    Err(_) => {
                        return CodemodeResult {
                            ok: false,
                            value: None,
                            error: Some("Return value is not JSON-serializable".to_string()),
                            items: std::mem::take(&mut *items.lock().unwrap()),
                        };
                    }
                }
            };
            CodemodeResult {
                ok: true,
                value: parsed_value,
                error: None,
                items: std::mem::take(&mut *items.lock().unwrap()),
            }
        }
    })
}
