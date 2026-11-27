use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures::future::BoxFuture;
use mehh_ai::ToolDefinition;
use rquickjs::FromJs;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub const CODEMODE_MAX_IMAGES: usize = 4;
pub const CODEMODE_MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_STORE_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_STORE_VALUE_BYTES: usize = 256 * 1024;
const MAX_STORE_KEY_BYTES: usize = 1024;

pub(crate) fn decoded_base64_len(data: &str) -> usize {
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take(2)
        .filter(|&&b| b == b'=')
        .count();
    ((data.len() / 4) * 3).saturating_sub(padding)
}

#[derive(Debug, Clone)]
pub struct CodemodeConfig {
    pub memory_limit_bytes: usize,
    pub timeout: Duration,
    pub max_output_chars: usize,
    pub max_output_items: usize,
    pub max_tool_calls: usize,
}

impl Default for CodemodeConfig {
    fn default() -> Self {
        Self {
            memory_limit_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(30),
            max_output_chars: 256 * 1024,
            max_output_items: 1024,
            max_tool_calls: 64,
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

impl From<&mehh_ai::Model> for ModelSummary {
    fn from(m: &mehh_ai::Model) -> Self {
        Self {
            id: m.id.clone(),
            name: m.name.clone(),
            provider: m.provider.clone(),
            api: m.api.clone(),
            context_window: m.context_window,
            reasoning: m.reasoning,
        }
    }
}

struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_byte_len(value: &serde_json::Value) -> Option<usize> {
    let mut writer = CountingWriter(0);
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.0)
}

static BUILTIN_MODELS_SUMMARY_JSON: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let models: Vec<ModelSummary> = mehh_ai::catalog::builtin_models_slice()
        .iter()
        .map(ModelSummary::from)
        .collect();
    serde_json::to_string(&models).unwrap_or_else(|_| "[]".to_string())
});

fn webp_signature_valid(data: &str) -> bool {
    if data.len() < 16 {
        return false;
    }
    let head = match BASE64.decode(&data.as_bytes()[..16]) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP"
}

/// Rust-side validation of image outputs. The JS prelude performs the same
/// checks, but user code can monkeypatch its built-ins, so this is authoritative.
fn validate_image_payload(data: &str, mime: &str) -> Result<(), String> {
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return Err("invalid image output: data is not valid base64".to_string());
    }
    let trimmed = data.trim_end_matches('=');
    if data.len() - trimmed.len() > 2
        || trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return Err("invalid image output: data is not valid base64".to_string());
    }
    let valid = match mime {
        "image/png" => data.starts_with("iVBORw0KGg"),
        "image/jpeg" => data.starts_with("/9j/"),
        "image/gif" => data.starts_with("R0lGOD"),
        "image/webp" => webp_signature_valid(data),
        _ => return Err(format!("unsupported output image type: {mime}")),
    };
    if !valid {
        return Err(format!("image data does not match declared type {mime}"));
    }
    Ok(())
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `exit()` sets the flag and lets QuickJS interrupt the VM. Only that specific
/// internal error counts as a clean exit: if user code catches the interrupt
/// and throws something else, the real error must still be reported.
fn is_clean_exit(exited: &AtomicBool, err: &str) -> bool {
    exited.load(Ordering::SeqCst) && err.contains("interrupted")
}

/// Cancels a child token when a tool call leaves scope, so a codemode timeout
/// cannot leave the underlying tool (bash, subagent, ...) running on the host.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub type SandboxToolExecutor = Arc<
    dyn Fn(&str, serde_json::Value, CancellationToken) -> BoxFuture<'static, Result<serde_json::Value, String>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResolvedToolMeta {
    pub(crate) name: String,
    pub(crate) js_name: String,
    pub(crate) description: String,
}

#[derive(Clone)]
pub(crate) struct PreparedToolMeta {
    pub(crate) defs: Arc<[ToolDefinition]>,
    pub(crate) resolved: Arc<[ResolvedToolMeta]>,
    pub(crate) meta_json: Arc<str>,
}

pub(crate) fn resolve_tool_meta(defs: Arc<[ToolDefinition]>) -> PreparedToolMeta {
    let mut used_js_names = std::collections::HashSet::with_capacity(defs.len());
    let resolved: Vec<ResolvedToolMeta> = defs
        .iter()
        .map(|t| {
            let base = to_identifier(&t.name);
            let mut js_name = base.clone();
            let mut suffix = 2;
            while !used_js_names.insert(js_name.clone()) {
                js_name = format!("{base}_{suffix}");
                suffix += 1;
            }
            ResolvedToolMeta {
                name: t.name.clone(),
                js_name,
                description: t.description.clone(),
            }
        })
        .collect();
    let json = serde_json::to_string(&resolved).unwrap_or_else(|_| "[]".to_string());
    PreparedToolMeta {
        defs,
        resolved: Arc::from(resolved),
        meta_json: Arc::from(json),
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
    const hostCallTool = globalThis.__host_call_tool;
    const hostLog = globalThis.__host_log;
    const hostText = globalThis.__host_text;
    const hostImage = globalThis.__host_image;
    const hostStore = globalThis.__host_store;
    const hostLoad = globalThis.__host_load;
    const hostExit = globalThis.__host_exit;
    const hostSearchTools = globalThis.__host_search_tools;
    const hostDescribeTool = globalThis.__host_describe_tool;
    const hostDescribeNamespace = globalThis.__host_describe_namespace;
    const hostModelsList = globalThis.__host_models_list;
    const hostModelsResolve = globalThis.__host_models_resolve;
    const hostModelsGetAvailable = globalThis.__host_models_get_available;
    // Capture pristine built-ins so user code cannot break the host bridge by
    // monkeypatching JSON, Map, Array or Object after the prelude has run.
    const jsonStringify = JSON.stringify;
    const jsonParse = JSON.parse;
    const mapGet = Function.prototype.call.bind(Map.prototype.get);
    const mapHas = Function.prototype.call.bind(Map.prototype.has);
    const mapSet = Function.prototype.call.bind(Map.prototype.set);
    const arrayFrom = Array.from;
    const objectFreeze = Object.freeze;
    for (const k of [
        "__host_call_tool",
        "__host_log",
        "__host_text",
        "__host_image",
        "__host_store",
        "__host_load",
        "__host_exit",
        "__host_search_tools",
        "__host_describe_tool",
        "__host_describe_namespace",
        "__host_models_list",
        "__host_models_resolve",
        "__host_models_get_available",
    ]) {
        delete globalThis[k];
    }

    const toolMap = new Map();
    const allTools = [];
    for (const t of __TOOLS_META__) {
        mapSet(toolMap, t.js_name, t.name);
        allTools.push(objectFreeze({ name: t.js_name, description: t.description }));
    }
    for (const t of __TOOLS_META__) {
        if (!mapHas(toolMap, t.name)) {
            mapSet(toolMap, t.name, t.name);
        }
    }
    const toolsProxy = new Proxy({}, {
        get(target, prop) {
            if (typeof prop !== "string" || prop === "then" || prop === "toJSON") {
                return undefined;
            }
            if (mapHas(toolMap, prop)) {
                const toolName = mapGet(toolMap, prop);
                return async function(args) {
                    const argsJson = jsonStringify(args !== undefined ? args : {});
                    const resJson = hostCallTool(toolName, argsJson);
                    let res;
                    try {
                        res = jsonParse(resJson);
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
            const available = arrayFrom(toolMap.keys());
            if (available.length > 0) {
                msg += " Available tools: " + available.join(", ") + ".";
            }
            msg += " ALL_TOOLS lists every available tool.";
            throw new TypeError(msg);
        },
        has(target, prop) {
            return mapHas(toolMap, prop);
        },
        ownKeys(target) {
            return arrayFrom(toolMap.keys());
        },
        getOwnPropertyDescriptor(target, prop) {
            if (mapHas(toolMap, prop)) {
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
            const json = jsonStringify(v);
            return json === undefined ? String(v) : json;
        } catch {
            return String(v);
        }
    }

    const consoleObj = {};
    for (const level of ["log", "info", "warn", "error", "debug"]) {
        consoleObj[level] = (...args) => {
            const formatted = args.map(outputText).join(" ");
            const err = hostLog(level, formatted);
            if (err) throw new RangeError(err);
        };
    }
    objectFreeze(consoleObj);

    function text(v) {
        const s = outputText(v);
        const err = hostText(s);
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
        const err = hostImage(data, detectedMime);
        if (err) throw new RangeError(err);
    }

    const MAX_STORE_VAL = 256 * 1024;
    function store(key, value) {
        if (typeof key !== "string") {
            throw new TypeError("store() key must be a string");
        }
        if (value === undefined) {
            const err = hostStore(key, "__UNDEFINED__");
            if (err) throw new RangeError(err);
            return;
        }
        let json;
        try {
            json = jsonStringify(value);
        } catch (e) {
            throw new TypeError("store('" + key + "') value is not JSON-serializable: " + e.message);
        }
        if (json === undefined) {
            throw new TypeError("store('" + key + "') value is not JSON-serializable");
        }
        if (json.length > MAX_STORE_VAL) {
            throw new RangeError("store('" + key + "') value exceeds 256KB limit");
        }
        const err = hostStore(key, json);
        if (err) throw new RangeError(err);
    }

    function load(key) {
        if (typeof key !== "string") {
            throw new TypeError("load() key must be a string");
        }
        const raw = hostLoad(key);
        if (raw === "" || raw === "__UNDEFINED__") {
            return undefined;
        }
        try {
            return jsonParse(raw);
        } catch {
            return undefined;
        }
    }

    function exit() {
        hostExit();
        // QuickJS polls its interrupt handler inside the interpreter loop and
        // marks the resulting exception uncatchable. Spin until that happens:
        // a plain `throw` could be caught by user code, which would let a
        // later error be reported as a successful exit.
        while (true) {}
    }

    async function searchTools(query, options) {
        if (typeof query !== "string") {
            throw new TypeError("searchTools(query) requires query to be a string");
        }
        const optJson = jsonStringify(options !== undefined ? options : {});
        const resJson = hostSearchTools(query, optJson);
        return jsonParse(resJson);
    }

    async function describeTool(name) {
        if (typeof name !== "string") {
            throw new TypeError("describeTool(name) requires name to be a string");
        }
        const raw = hostDescribeTool(name);
        if (!raw) {
            return undefined;
        }
        return raw;
    }

    async function describeNamespace(name) {
        if (typeof name !== "string") {
            throw new TypeError("describeNamespace(name) requires name to be a string");
        }
        const raw = hostDescribeNamespace(name);
        if (!raw) {
            return undefined;
        }
        return jsonParse(raw);
    }

    const modelsObj = {
        list() {
            return jsonParse(hostModelsList());
        },
        resolve(providerOrId, modelId) {
            const mIdJson = jsonStringify(modelId !== undefined ? modelId : null);
            const res = hostModelsResolve(String(providerOrId), mIdJson);
            if (!res) {
                return undefined;
            }
            return jsonParse(res);
        },
        getAvailableOfType(provider) {
            const pJson = jsonStringify(provider !== undefined ? provider : null);
            return jsonParse(hostModelsGetAvailable(pJson));
        }
    };
    objectFreeze(modelsObj);

    globalThis.tools = toolsProxy;
    globalThis.ALL_TOOLS = objectFreeze(allTools);
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
    run_codemode_shared(
        code,
        Arc::from(tool_defs),
        executor,
        kv_store,
        config,
        cancel,
    )
    .await
}

pub async fn run_codemode_shared(
    code: &str,
    tool_defs: Arc<[ToolDefinition]>,
    executor: SandboxToolExecutor,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: &CodemodeConfig,
    cancel: CancellationToken,
) -> CodemodeResult {
    let prepared = resolve_tool_meta(tool_defs);
    run_codemode_with_meta(code, prepared, executor, kv_store, config, cancel).await
}

pub(crate) async fn run_codemode_with_meta(
    code: &str,
    prepared: PreparedToolMeta,
    executor: SandboxToolExecutor,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: &CodemodeConfig,
    cancel: CancellationToken,
) -> CodemodeResult {
    let code = code.to_string();
    let config = config.clone();
    let handle = tokio::runtime::Handle::current();
    let run_cancel = cancel.child_token();
    let _cancel_guard = CancelOnDrop(run_cancel.clone());

    tokio::task::spawn_blocking(move || {
        run_codemode_blocking(
            &code,
            prepared,
            executor,
            kv_store,
            &config,
            run_cancel,
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
    prepared: PreparedToolMeta,
    executor: SandboxToolExecutor,
    kv_store: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    config: &CodemodeConfig,
    cancel: CancellationToken,
    handle: tokio::runtime::Handle,
) -> CodemodeResult {
    let tool_defs = Arc::clone(&prepared.defs);
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
    // Trigger GC well before the hard memory limit so a script cannot fill
    // the heap with short-lived values and get killed without a chance to run
    // its own cleanup path.
    rt.set_gc_threshold(256 * 1024);
    // Bound the native stack so deep JS recursion raises InternalError instead
    // of aborting the whole process from a spawn_blocking worker thread.
    rt.set_max_stack_size(512 * 1024);

    let exited = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + config.timeout;
    let cancel_check = cancel.clone();
    let interrupt_exited = exited.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        interrupt_exited.load(Ordering::Relaxed)
            || Instant::now() > deadline
            || cancel_check.is_cancelled()
    })));

    // Pre-validate user source in a bare QuickJS context with zero host
    // bindings or prelude. QuickJS's AsyncFunction constructor parses via a
    // synthesized wrapper where a balanced `})()` sequence could run before
    // `toString` verification; validating first in an unprivileged context
    // guarantees any escape attempt has zero capabilities.
    let code_json = serde_json::to_string(code).unwrap_or_else(|_| "\"\"".to_string());
    {
        let precheck_ctx = match rquickjs::Context::full(&rt) {
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
        let precheck_err = precheck_ctx.with(|ctx| -> Option<String> {
            let check_code = format!(
                "((__src) => {{ const pristineToString = Function.prototype.toString; const AsyncFunction = Object.getPrototypeOf(async function() {{}}).constructor; const __f = AsyncFunction(__src); const __expected = \"async function anonymous(\\n) {{\\n\" + __src + \"\\n}}\"; if (pristineToString.call(__f) !== __expected) {{ throw new Error(\"Invalid async wrapper source\"); }} }})({code_json})"
            );
            match ctx.eval::<(), _>(check_code.as_str()) {
                Ok(()) => None,
                Err(err) => {
                    if Instant::now() > deadline {
                        return Some("Execution timed out".to_string());
                    }
                    if cancel.is_cancelled() {
                        return Some("Execution was cancelled".to_string());
                    }
                    let exc = ctx.catch();
                    Some(format_js_error(&ctx, exc, err))
                }
            }
        });
        if let Some(err_msg) = precheck_err {
            return CodemodeResult {
                ok: false,
                value: None,
                error: Some(err_msg),
                items: Vec::new(),
            };
        }
    }

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
    let tool_calls = Arc::new(AtomicUsize::new(0));

    context.with(|ctx| {
        let globals = ctx.globals();

        // 1. __host_call_tool
        let handle_tool = handle.clone();
        let executor_tool = executor.clone();
        let cancel_tool = cancel.clone();
        let calls = tool_calls.clone();
        let max_tool_calls = config.max_tool_calls;
        let host_call_tool = match rquickjs::Function::new(
            ctx.clone(),
            move |name: String, args_json: String| -> String {
                if cancel_tool.is_cancelled() {
                    return serde_json::json!({ "ok": false, "error": "Execution was cancelled" }).to_string();
                }
                let now = Instant::now();
                if now >= deadline {
                    return serde_json::json!({ "ok": false, "error": "Execution timed out" }).to_string();
                }
                let seq = calls.fetch_add(1, Ordering::Relaxed) + 1;
                if seq > max_tool_calls {
                    return serde_json::json!({
                        "ok": false,
                        "error": format!("script exceeded the {max_tool_calls} tool-call limit")
                    })
                    .to_string();
                }
                let args_val: serde_json::Value = match serde_json::from_str(&args_json) {
                    Ok(v) => v,
                    Err(e) => {
                        return serde_json::json!({
                            "ok": false,
                            "error": format!("invalid tool arguments: {e}")
                        })
                        .to_string();
                    }
                };
                let remaining = deadline.saturating_duration_since(now);
                // A child token is cancelled on every exit path (including the
                // sleep/timeout branch) so nested bash/subagent tools cannot
                // outlive the codemode call that started them.
                let call_cancel = cancel_tool.child_token();
                let _cancel_on_drop = CancelOnDrop(call_cancel.clone());
                let res = handle_tool.block_on(async {
                    tokio::select! {
                        _ = call_cancel.cancelled() => Err("Execution was cancelled".to_string()),
                        _ = tokio::time::sleep(remaining) => Err("Execution timed out".to_string()),
                        out = (executor_tool)(&name, args_val, call_cancel.clone()) => out,
                    }
                });
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
                lock_or_recover(&items_log)
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
                lock_or_recover(&items_text)
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
        let total_items_image = total_items.clone();
        let image_count = Arc::new(AtomicUsize::new(0));
        let total_image_bytes = Arc::new(AtomicUsize::new(0));
        let host_image = match rquickjs::Function::new(
            ctx.clone(),
            move |data: String, mime: String| -> String {
                if let Err(err) = validate_image_payload(&data, &mime) {
                    return err;
                }
                let decoded_len = decoded_base64_len(&data);
                if decoded_len > CODEMODE_MAX_IMAGE_BYTES {
                    return format!(
                        "Output image exceeds the {} MiB limit",
                        CODEMODE_MAX_IMAGE_BYTES / (1024 * 1024)
                    );
                }
                let current_images = image_count.load(Ordering::Relaxed);
                if current_images >= CODEMODE_MAX_IMAGES {
                    return format!("Too many output images (maximum {CODEMODE_MAX_IMAGES})");
                }
                let new_image_bytes = total_image_bytes
                    .load(Ordering::Relaxed)
                    .saturating_add(decoded_len);
                if new_image_bytes > CODEMODE_MAX_IMAGES * CODEMODE_MAX_IMAGE_BYTES {
                    return "script output exceeded image output limit".to_string();
                }
                let new_items = total_items_image.load(Ordering::Relaxed) + 1;
                if new_items > max_items {
                    return "script output exceeded output limit".to_string();
                }
                image_count.store(current_images + 1, Ordering::Relaxed);
                total_image_bytes.store(new_image_bytes, Ordering::Relaxed);
                total_items_image.store(new_items, Ordering::Relaxed);
                lock_or_recover(&items_image).push(CodemodeOutputItem::Image {
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
        struct StoreAccounting {
            key_sizes: HashMap<String, usize>,
            total_bytes: usize,
        }
        let kv_store_set = kv_store.clone();
        let cancel_store = cancel.clone();
        let store_accounting: Arc<Mutex<Option<StoreAccounting>>> = Arc::new(Mutex::new(None));
        let host_store = match rquickjs::Function::new(
            ctx.clone(),
            move |key: String, val_json: String| -> String {
                if cancel_store.is_cancelled() {
                    return "Execution was cancelled".to_string();
                }
                if Instant::now() > deadline {
                    return "Execution timed out".to_string();
                }
                if key.len() > MAX_STORE_KEY_BYTES {
                    return format!("store() key exceeds {MAX_STORE_KEY_BYTES} byte limit");
                }
                let mut map = lock_or_recover(&kv_store_set);
                let mut accounting_guard = lock_or_recover(&store_accounting);
                let accounting = accounting_guard.get_or_insert_with(|| {
                    let mut key_sizes = HashMap::with_capacity(map.len());
                    let mut total_bytes = 0usize;
                    for (k, v) in map.iter() {
                        let sz = k.len() + json_byte_len(v).unwrap_or(0);
                        key_sizes.insert(k.clone(), sz);
                        total_bytes = total_bytes.saturating_add(sz);
                    }
                    StoreAccounting {
                        key_sizes,
                        total_bytes,
                    }
                });
                if val_json == "__UNDEFINED__" {
                    map.remove(&key);
                    if let Some(old) = accounting.key_sizes.remove(&key) {
                        accounting.total_bytes = accounting.total_bytes.saturating_sub(old);
                    }
                    return String::new();
                }
                if val_json.len() > MAX_STORE_VALUE_BYTES {
                    return format!("store('{key}') value exceeds 256KB limit");
                }
                let val: serde_json::Value = match serde_json::from_str(&val_json) {
                    Ok(v) => v,
                    Err(e) => return format!("store('{key}') invalid JSON: {e}"),
                };
                let canonical_len = json_byte_len(&val).unwrap_or(val_json.len());
                let new_bytes = key.len() + canonical_len;
                let old_bytes = accounting.key_sizes.get(&key).copied().unwrap_or(0);
                let projected = accounting
                    .total_bytes
                    .saturating_sub(old_bytes)
                    .saturating_add(new_bytes);
                if projected > MAX_STORE_TOTAL_BYTES {
                    return "store is full: stored values would exceed 1MB limit".to_string();
                }
                map.insert(key.clone(), val);
                accounting.key_sizes.insert(key, new_bytes);
                accounting.total_bytes = projected;
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
                let map = lock_or_recover(&kv_store_get);
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

        // Reuse pre-resolved deduplicated JS identifiers and JSON metadata.
        let resolved_tools = Arc::clone(&prepared.resolved);
        let tools_meta_json = Arc::clone(&prepared.meta_json);
        let shared_tool_defs = tool_defs;

        // 8. __host_search_tools
        let tool_defs_search = Arc::clone(&shared_tool_defs);
        let resolved_search = Arc::clone(&resolved_tools);
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

                let ns_filter = opts.namespace.as_deref().map(|ns| {
                    if ns == "builtin" {
                        (ns, None)
                    } else {
                        let ns_prefix = format!("mcp__{ns}__");
                        let ns_sanitized = ns.replace('-', "_");
                        let ns_prefix_sanitized = format!("mcp__{ns_sanitized}__");
                        (ns, Some((ns_prefix, ns_prefix_sanitized)))
                    }
                });

                let mut scored: Vec<(f64, usize)> = Vec::new();

                for (idx, tool) in tool_defs_search.iter().enumerate() {
                    if let Some((ns, ref prefixes)) = ns_filter {
                        if let Some((ns_prefix, ns_prefix_sanitized)) = prefixes {
                            let exact = tool.name == ns;
                            if !exact
                                && !tool.name.starts_with(ns_prefix)
                                && !tool.name.starts_with(ns_prefix_sanitized)
                                && !tool.name.starts_with(ns)
                            {
                                continue;
                            }
                        } else if tool.name.starts_with("mcp__") {
                            continue;
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
                        scored.push((score, idx));
                    }
                }

                scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

                let results: Vec<serde_json::Value> = scored
                    .into_iter()
                    .take(limit)
                    .map(|(_, idx)| {
                        serde_json::json!({
                            "name": resolved_search[idx].js_name.clone(),
                            "description": resolved_search[idx].description.clone()
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
        let tool_defs_desc = Arc::clone(&shared_tool_defs);
        let resolved_desc = Arc::clone(&resolved_tools);
        let host_describe_tool = match rquickjs::Function::new(
            ctx.clone(),
            move |name: String| -> String {
                let found = resolved_desc
                    .iter()
                    .position(|m| m.js_name == name)
                    .or_else(|| resolved_desc.iter().position(|m| m.name == name));
                match found {
                    Some(idx) => {
                        let t = &tool_defs_desc[idx];
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
        let resolved_ns = Arc::clone(&resolved_tools);
        let host_describe_namespace = match rquickjs::Function::new(
            ctx.clone(),
            move |name: String| -> String {
                let tools: Vec<String> = if name == "builtin" {
                    resolved_ns
                        .iter()
                        .filter(|t| !t.name.starts_with("mcp__"))
                        .map(|t| t.js_name.clone())
                        .collect()
                } else {
                    let prefix = format!("mcp__{name}__");
                    let sanitized = name.replace('-', "_");
                    let prefix_sanitized = format!("mcp__{sanitized}__");
                    resolved_ns
                        .iter()
                        .filter(|t| {
                            t.name.starts_with(&prefix)
                                || t.name.starts_with(&prefix_sanitized)
                                || t.name == name
                        })
                        .map(|t| t.js_name.clone())
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
        let host_models_list = match rquickjs::Function::new(ctx.clone(), move || -> String {
            BUILTIN_MODELS_SUMMARY_JSON.clone()
        }) {
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
                let filtered: Vec<ModelSummary> = mehh_ai::catalog::builtin_models_slice()
                    .iter()
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

        // Setup prelude with the precomputed deduplicated metadata table.
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

        // Evaluate user code. The source is passed as a JSON string literal to
        // an AsyncFunction constructor. The constructor re-evals a synthesized
        // wrapper, so a balanced `})()` sequence in user code can break out of
        // that wrapper and make the constructor return an attacker-chosen
        // function instead of throwing. Capture a pristine toString first, then
        // verify the constructed function's source is exactly the expected
        // wrapper with our body before invoking it.
        let wrapped_code = format!(
            "((__src) => {{ const pristineToString = Function.prototype.toString; const AsyncFunction = Object.getPrototypeOf(async function() {{}}).constructor; const __f = AsyncFunction(__src); const __expected = \"async function anonymous(\\n) {{\\n\" + __src + \"\\n}}\"; if (pristineToString.call(__f) !== __expected) {{ throw new Error(\"Invalid async wrapper source\"); }} return __f(); }})({code_json})"
        );
        let eval_val: rquickjs::Value = match ctx.eval(wrapped_code.as_str()) {
            Ok(v) => v,
            Err(err) => {
                if Instant::now() > deadline {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution timed out".to_string()),
                        items: std::mem::take(&mut *lock_or_recover(&items)),
                    };
                }
                if cancel.is_cancelled() {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution was cancelled".to_string()),
                        items: std::mem::take(&mut *lock_or_recover(&items)),
                    };
                }
                let exc = ctx.catch();
                let err_msg = format_js_error(&ctx, exc, err);
                if is_clean_exit(&exited, &err_msg) {
                    return CodemodeResult {
                        ok: true,
                        value: None,
                        error: None,
                        items: std::mem::take(&mut *lock_or_recover(&items)),
                    };
                }
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(err_msg),
                    items: std::mem::take(&mut *lock_or_recover(&items)),
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
                    items: std::mem::take(&mut *lock_or_recover(&items)),
                };
            }
            if cancel.is_cancelled() {
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some("Execution was cancelled".to_string()),
                    items: std::mem::take(&mut *lock_or_recover(&items)),
                };
            }
            // Exit is an uncatchable QuickJS interrupt, so a rejected promise
            // can still be a clean exit. Any other rejection is a real error.
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
                                items: std::mem::take(&mut *lock_or_recover(&items)),
                            };
                        }
                        None => rquickjs::Value::new_undefined(ctx.clone()),
                    };

                    finalize_codemode_value(
                        &ctx,
                        &val,
                        deadline,
                        &cancel,
                        &exited,
                        config.max_output_chars,
                        &items,
                    )
                }
                rquickjs::promise::PromiseState::Rejected => {
                    let _ = promise.result::<rquickjs::Value>();
                    let exc = ctx.catch();
                    let err_msg = format_js_error(&ctx, exc, rquickjs::Error::Exception);
                    if is_clean_exit(&exited, &err_msg) {
                        return CodemodeResult {
                            ok: true,
                            value: None,
                            error: None,
                            items: std::mem::take(&mut *lock_or_recover(&items)),
                        };
                    }
                    CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some(err_msg),
                        items: std::mem::take(&mut *lock_or_recover(&items)),
                    }
                }
                rquickjs::promise::PromiseState::Pending => {
                    // An uncatchable internal error (OOM, stack overflow) or exit
                    // raised inside a promise job can leave the promise pending
                    // with a pending exception on the context.
                    let exc = ctx.catch();
                    if !exc.is_null() && !exc.is_undefined() {
                        let err_msg = format_js_error(&ctx, exc, rquickjs::Error::Exception);
                        if is_clean_exit(&exited, &err_msg) {
                            return CodemodeResult {
                                ok: true,
                                value: None,
                                error: None,
                                items: std::mem::take(&mut *lock_or_recover(&items)),
                            };
                        }
                        return CodemodeResult {
                            ok: false,
                            value: None,
                            error: Some(err_msg),
                            items: std::mem::take(&mut *lock_or_recover(&items)),
                        };
                    }
                    if exited.load(Ordering::SeqCst) {
                        return CodemodeResult {
                            ok: true,
                            value: None,
                            error: None,
                            items: std::mem::take(&mut *lock_or_recover(&items)),
                        };
                    }
                    CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some(
                            "The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here."
                                .to_string(),
                        ),
                        items: std::mem::take(&mut *lock_or_recover(&items)),
                    }
                }
            }
        } else {
            // A plain value can only appear if QuickJS fails to construct the
            // async function; still apply the same deadline/cancel/exit checks.
            finalize_codemode_value(
                &ctx,
                &eval_val,
                deadline,
                &cancel,
                &exited,
                config.max_output_chars,
                &items,
            )
        }
    })
}

fn finalize_codemode_value<'js>(
    ctx: &rquickjs::Ctx<'js>,
    val: &rquickjs::Value<'js>,
    deadline: Instant,
    cancel: &CancellationToken,
    exited: &AtomicBool,
    max_output_chars: usize,
    items: &Mutex<Vec<CodemodeOutputItem>>,
) -> CodemodeResult {
    if Instant::now() > deadline {
        return CodemodeResult {
            ok: false,
            value: None,
            error: Some("Execution timed out".to_string()),
            items: std::mem::take(&mut *lock_or_recover(items)),
        };
    }
    if cancel.is_cancelled() {
        return CodemodeResult {
            ok: false,
            value: None,
            error: Some("Execution was cancelled".to_string()),
            items: std::mem::take(&mut *lock_or_recover(items)),
        };
    }
    if exited.load(Ordering::SeqCst) {
        return CodemodeResult {
            ok: true,
            value: None,
            error: None,
            items: std::mem::take(&mut *lock_or_recover(items)),
        };
    }
    let parsed_value = if val.is_undefined() {
        None
    } else {
        match ctx.json_stringify(val) {
            Ok(Some(s)) => {
                if Instant::now() > deadline {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution timed out".to_string()),
                        items: std::mem::take(&mut *lock_or_recover(items)),
                    };
                }
                if cancel.is_cancelled() {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution was cancelled".to_string()),
                        items: std::mem::take(&mut *lock_or_recover(items)),
                    };
                }
                let json_str = match s.to_string() {
                    Ok(s) => s,
                    Err(e) => {
                        return CodemodeResult {
                            ok: false,
                            value: None,
                            error: Some(format!("Return value is not valid UTF-8: {e}")),
                            items: std::mem::take(&mut *lock_or_recover(items)),
                        };
                    }
                };
                if json_str.len() > max_output_chars {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some(format!(
                            "Return value exceeds maximum output size ({} > {} bytes)",
                            json_str.len(),
                            max_output_chars
                        )),
                        items: std::mem::take(&mut *lock_or_recover(items)),
                    };
                }
                match serde_json::from_str::<serde_json::Value>(&json_str) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        return CodemodeResult {
                            ok: false,
                            value: None,
                            error: Some(format!("Return value is not valid JSON: {e}")),
                            items: std::mem::take(&mut *lock_or_recover(items)),
                        };
                    }
                }
            }
            Ok(None) => None,
            Err(e) => {
                if Instant::now() > deadline {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution timed out".to_string()),
                        items: std::mem::take(&mut *lock_or_recover(items)),
                    };
                }
                if cancel.is_cancelled() {
                    return CodemodeResult {
                        ok: false,
                        value: None,
                        error: Some("Execution was cancelled".to_string()),
                        items: std::mem::take(&mut *lock_or_recover(items)),
                    };
                }
                let exc = ctx.catch();
                let detail = if !exc.is_null() && !exc.is_undefined() {
                    format_js_error(ctx, exc, e)
                } else {
                    e.to_string()
                };
                return CodemodeResult {
                    ok: false,
                    value: None,
                    error: Some(format!("Return value is not JSON-serializable: {detail}")),
                    items: std::mem::take(&mut *lock_or_recover(items)),
                };
            }
        }
    };
    CodemodeResult {
        ok: true,
        value: parsed_value,
        error: None,
        items: std::mem::take(&mut *lock_or_recover(items)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png_base64(decoded_bytes: usize) -> String {
        let mut raw = vec![0u8; decoded_bytes.max(8)];
        raw[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        BASE64.encode(&raw)
    }

    fn noop_executor() -> SandboxToolExecutor {
        Arc::new(|_name, _args, _cancel| Box::pin(async { Ok(serde_json::json!("ok")) }))
    }

    #[tokio::test]
    async fn test_large_image_above_256kib_succeeds_under_default_config() {
        // ~1.53 MiB decoded -> ~2.03 MiB base64 (> 256 KiB default max_output_chars and
        // > 2 MiB raw base64 chars, but < 2 MiB decoded CODEMODE_MAX_IMAGE_BYTES).
        let b64 = make_png_base64(1_600_000);
        assert!(b64.len() > CODEMODE_MAX_IMAGE_BYTES);
        let code = format!(r#"image("data:image/png;base64,{b64}"); return "ok";"#);
        let res = run_codemode(
            &code,
            &[],
            noop_executor(),
            Arc::new(Mutex::new(HashMap::new())),
            &CodemodeConfig::default(),
            CancellationToken::new(),
        )
        .await;
        assert!(res.ok, "expected >256 KiB image to succeed, got: {:?}", res.error);
        assert_eq!(res.items.len(), 1);
        assert!(matches!(res.items[0], CodemodeOutputItem::Image { .. }));

        // Exceeding decoded CODEMODE_MAX_IMAGE_BYTES must still be rejected.
        let oversized_b64 = make_png_base64(CODEMODE_MAX_IMAGE_BYTES + 3);
        let oversized_code = format!(r#"image("data:image/png;base64,{oversized_b64}");"#);
        let oversized_res = run_codemode(
            &oversized_code,
            &[],
            noop_executor(),
            Arc::new(Mutex::new(HashMap::new())),
            &CodemodeConfig::default(),
            CancellationToken::new(),
        )
        .await;
        assert!(!oversized_res.ok);
    }

    #[tokio::test]
    async fn test_rejected_image_does_not_burn_quota() {
        let valid_b64 = make_png_base64(64);
        let oversized_b64 = make_png_base64(CODEMODE_MAX_IMAGE_BYTES + 3);
        let code = format!(
            r#"
            for (let i = 0; i < 5; i++) {{
                try {{ image("data:image/png;base64,{oversized_b64}"); }} catch (e) {{}}
            }}
            for (let i = 0; i < {CODEMODE_MAX_IMAGES}; i++) {{
                image("data:image/png;base64,{valid_b64}");
            }}
            return "ok";
            "#
        );
        let res = run_codemode(
            &code,
            &[],
            noop_executor(),
            Arc::new(Mutex::new(HashMap::new())),
            &CodemodeConfig::default(),
            CancellationToken::new(),
        )
        .await;
        assert!(res.ok, "rejected images must not burn quota, got: {:?}", res.error);
        assert_eq!(res.items.len(), CODEMODE_MAX_IMAGES);
    }

    #[tokio::test]
    async fn test_dropping_run_codemode_future_cancels_immediately() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let cancelled_tx = Arc::new(Mutex::new(Some(cancelled_tx)));

        let executor: SandboxToolExecutor = Arc::new(move |_name, _args, cancel| {
            let started_tx = started_tx.clone();
            let cancelled_tx = cancelled_tx.clone();
            Box::pin(async move {
                if let Some(tx) = lock_or_recover(&started_tx).take() {
                    let _ = tx.send(());
                }
                cancel.cancelled().await;
                if let Some(tx) = lock_or_recover(&cancelled_tx).take() {
                    let _ = tx.send(());
                }
                Err("cancelled".to_string())
            })
        });

        let tool_def = ToolDefinition {
            name: "slow".to_string(),
            description: "Slow tool".to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        };
        let kv = Arc::new(Mutex::new(HashMap::new()));
        let config = CodemodeConfig {
            timeout: Duration::from_secs(30),
            ..Default::default()
        };

        {
            let tool_defs = [tool_def];
            let mut fut = Box::pin(run_codemode(
                r#"await tools.slow({});"#,
                &tool_defs,
                executor,
                kv,
                &config,
                CancellationToken::new(),
            ));
            tokio::select! {
                _ = &mut fut => panic!("run_codemode should still be awaiting slow tool"),
                _ = started_rx => {}
            }
        }

        tokio::time::timeout(Duration::from_secs(2), cancelled_rx)
            .await
            .expect("nested tool cancellation did not fire promptly after future drop")
            .expect("cancelled_tx dropped unexpectedly");
    }

    #[tokio::test]
    async fn test_post_await_stack_overflow_and_oom_report_real_error() {
        let tool_def = ToolDefinition {
            name: "echo".to_string(),
            description: "Echo".to_string(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        };

        // 1. Stack overflow after await
        let stack_res = run_codemode(
            r#"await tools.echo({}); function f() { return f(); } f();"#,
            std::slice::from_ref(&tool_def),
            noop_executor(),
            Arc::new(Mutex::new(HashMap::new())),
            &CodemodeConfig::default(),
            CancellationToken::new(),
        )
        .await;
        assert!(!stack_res.ok);
        let stack_err = stack_res.error.unwrap_or_default();
        assert!(
            !stack_err.contains("promise that can never settle"),
            "stack overflow after await must not masquerade as unsettled promise: {stack_err}"
        );
        assert!(
            stack_err.to_lowercase().contains("stack")
                || stack_err.contains("InternalError"),
            "expected stack overflow error, got: {stack_err}"
        );

        // 2. OOM after await
        let oom_config = CodemodeConfig {
            memory_limit_bytes: 2 * 1024 * 1024,
            timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let oom_res = run_codemode(
            r#"await tools.echo({}); const a = []; while (true) { a.push("x".repeat(65536)); }"#,
            std::slice::from_ref(&tool_def),
            noop_executor(),
            Arc::new(Mutex::new(HashMap::new())),
            &oom_config,
            CancellationToken::new(),
        )
        .await;
        assert!(!oom_res.ok);
        let oom_err = oom_res.error.unwrap_or_default();
        assert!(
            !oom_err.contains("promise that can never settle"),
            "OOM after await must not masquerade as unsettled promise: {oom_err}"
        );
        assert!(
            oom_err.to_lowercase().contains("memory")
                || oom_err.contains("InternalError"),
            "expected OOM error, got: {oom_err}"
        );
    }

    #[tokio::test]
    async fn test_oversized_return_value_is_rejected() {
        let config = CodemodeConfig {
            max_output_chars: 1024,
            ..Default::default()
        };
        let res = run_codemode(
            r#"return "a".repeat(2048);"#,
            &[],
            noop_executor(),
            Arc::new(Mutex::new(HashMap::new())),
            &config,
            CancellationToken::new(),
        )
        .await;
        assert!(!res.ok, "oversized return value must fail");
        assert_eq!(res.value, None);
        let err = res.error.unwrap_or_default();
        assert!(
            err.contains("exceeds maximum output size"),
            "unexpected error message: {err}"
        );
    }
}
