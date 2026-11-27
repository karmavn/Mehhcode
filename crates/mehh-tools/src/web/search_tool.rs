//! Built-in `web_search` AgentTool backed by the keyless failover router.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mehh_agent::{AgentTool, AgentToolResult, AgentToolUpdateCallback, ToolExecutionMode};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use super::output::format_search_results;
use super::router::ProgressFn;
use super::types::WebProviderChoice;
use super::{LazyWebRuntime, WebRuntime, WebRuntimeHandle};

/// Built-in web search tool using Mehhcode's 6 keyless search providers.
#[derive(Clone)]
pub struct WebSearchTool {
    runtime: WebRuntimeHandle,
    description: String,
}

impl WebSearchTool {
    pub fn new(runtime: Result<Arc<WebRuntime>, String>) -> Self {
        Self::with_handle(WebRuntimeHandle::Eager(runtime))
    }

    pub fn from_runtime(runtime: Arc<WebRuntime>) -> Self {
        Self::new(Ok(runtime))
    }

    pub fn lazy(runtime: Arc<LazyWebRuntime>) -> Self {
        Self::with_handle(WebRuntimeHandle::Lazy(runtime))
    }

    fn with_handle(runtime: WebRuntimeHandle) -> Self {
        let year = current_utc_year();
        let description = format!(
            "Search the live web for real-time information (weather, news, latest releases), documentation, and external projects/repositories beyond your knowledge cutoff. Use this whenever a question requires live or external information rather than local workspace files. Returns concise markdown search results.\n\nThe current year is {year}. Include {year} when querying recent releases or current events."
        );
        Self {
            runtime,
            description,
        }
    }
}

fn current_utc_year() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant's civil_from_days: extracts Gregorian year from Unix day offset.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    y + i64::from(m <= 2)
}

#[async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn label(&self) -> &str {
        "Web Search"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Web search query"
                }
            },
            "required": ["query"]
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn is_read_only(&self) -> Option<bool> {
        Some(true)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Map<String, Value>,
        cancel: CancellationToken,
        on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, String> {
        if cancel.is_cancelled() {
            return Err("Web search aborted".to_string());
        }
        let runtime = self.runtime.resolve()?;
        if !runtime.config.enabled || runtime.config.provider == WebProviderChoice::Disabled {
            return Err("Web search is disabled in settings".to_string());
        }

        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Missing required 'query' parameter".to_string())?;

        let progress_cb: Option<Box<ProgressFn>> = on_update.map(|cb| {
            let f: Box<ProgressFn> = Box::new(move |msg: &str| {
                cb(AgentToolResult::text(msg.to_string()));
            });
            f
        });

        let timeout = Duration::from_secs(runtime.config.effective_timeout_seconds());
        let output = runtime
            .router
            .search(
                runtime.config.provider,
                query,
                timeout,
                &cancel,
                progress_cb.as_deref(),
            )
            .await
            .map_err(|e| format!("Unable to search the web for '{query}': {e}"))?;

        let result_count = output.hits.len();
        let formatted = format_search_results(&output.hits);
        Ok(AgentToolResult {
            content: vec![mehh_ai::ToolResultContentBlock::Text(mehh_ai::TextContent {
                text: formatted.text,
                text_signature: None,
            })],
            details: Some(json!({
                "provider": output.provider.as_str(),
                "resultCount": result_count,
                "cached": output.cached,
            })),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::{WebConfig, WebEndpoints};
    use mehh_ai::ToolResultContentBlock;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_web_search_tool_contract_and_progress() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = json!({
                "results": [{
                    "title": "Tokio Docs",
                    "url": "https://tokio.rs",
                    "content": "Async runtime"
                }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });

        let mut cfg = WebConfig {
            provider: WebProviderChoice::Fixed(crate::web::ProviderId::Tavily),
            endpoints: WebEndpoints::production(),
            ..Default::default()
        };
        cfg.endpoints.tavily_search = format!("http://{addr}/search");
        let tool = WebSearchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));

        assert_eq!(tool.name(), "web_search");
        assert_eq!(tool.is_read_only(), Some(true));
        assert!(tool.description().contains(&current_utc_year().to_string()));

        let updates = Arc::new(Mutex::new(Vec::<String>::new()));
        let upd_clone = updates.clone();
        let cb: AgentToolUpdateCallback = Arc::new(move |res| {
            if let Some(ToolResultContentBlock::Text(t)) = res.content.first() {
                upd_clone.lock().unwrap().push(t.text.clone());
            }
        });

        let mut params = Map::new();
        params.insert("query".into(), json!("tokio rust"));
        let res = tool
            .execute("tc_ws", params, CancellationToken::new(), Some(cb))
            .await
            .unwrap();

        let text = match &res.content[0] {
            ToolResultContentBlock::Text(t) => t.text.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("[Tokio Docs](https://tokio.rs)"));
        assert_eq!(res.details.as_ref().unwrap()["provider"], "tavily");
        assert_eq!(res.details.as_ref().unwrap()["resultCount"], 1);
        assert_eq!(res.details.as_ref().unwrap()["cached"], false);
        assert!(updates
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("Searching via Tavily")));
    }

    #[tokio::test]
    async fn test_web_search_disabled_returns_error() {
        let cfg = WebConfig {
            enabled: false,
            ..Default::default()
        };
        let tool = WebSearchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg)));
        let mut params = Map::new();
        params.insert("query".into(), json!("anything"));
        assert!(tool
            .execute("tc_dis", params, CancellationToken::new(), None)
            .await
            .is_err());

        let cfg2 = WebConfig {
            provider: WebProviderChoice::Disabled,
            ..Default::default()
        };
        let tool2 = WebSearchTool::from_runtime(Arc::new(WebRuntime::for_tests(cfg2)));
        let mut params2 = Map::new();
        params2.insert("query".into(), json!("anything"));
        assert!(tool2
            .execute("tc_dis2", params2, CancellationToken::new(), None)
            .await
            .is_err());
    }
}
