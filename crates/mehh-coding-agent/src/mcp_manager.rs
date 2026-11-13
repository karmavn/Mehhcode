//! MCP server connection management, tool lifecycle, and summaries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use mehh_agent::AgentTool;
use mehh_mcp::{
    load_mcp_configs, ListMcpResourceTemplatesTool, ListMcpResourcesTool, McpAgentTool, McpAuthEntry,
    McpAuthStore, McpClient, McpRoot, McpServerConfig, McpServerRegistry, McpToolInfo,
    McpTransport, OAuthTokens, ReadMcpResourceTool, StdioTransport, StoredMcpBearerProvider,
    StreamableHttpTransport,
};

/// Connection status summary for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConnectionSummary {
    pub server: String,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// Converts a local filesystem path to a standard `file://` URI.
pub fn cwd_to_root_uri(cwd: &Path) -> String {
    let abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let path_str = abs.to_string_lossy().replace('\\', "/");
    let clean = path_str.trim_start_matches("//?/");
    if clean.starts_with('/') {
        format!("file://{clean}")
    } else {
        format!("file:///{clean}")
    }
}

/// Manages MCP client connections, active MCP tools, resource meta-tools, and OAuth storage.
pub struct McpManager {
    cwd: PathBuf,
    configs: HashMap<String, McpServerConfig>,
    clients: Vec<(String, Arc<McpClient>)>,
    tools: Vec<Arc<dyn AgentTool>>,
    resource_tools: Vec<Arc<dyn AgentTool>>,
    summaries: Vec<McpConnectionSummary>,
    registry: McpServerRegistry,
    auth_store: Arc<McpAuthStore>,
}

impl Default for McpManager {
    fn default() -> Self {
        let registry: McpServerRegistry = Arc::new(tokio::sync::RwLock::new(Vec::new()));
        let auth_store = Arc::new(McpAuthStore::new());
        let resource_tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(ListMcpResourcesTool::new(registry.clone())),
            Arc::new(ListMcpResourceTemplatesTool::new(registry.clone())),
            Arc::new(ReadMcpResourceTool::new(registry.clone())),
        ];
        Self {
            cwd: PathBuf::new(),
            configs: HashMap::new(),
            clients: Vec::new(),
            tools: Vec::new(),
            resource_tools,
            summaries: Vec::new(),
            registry,
            auth_store,
        }
    }
}

impl McpManager {
    /// Loads MCP configs from disk (global and project) and attempts connection to each enabled server.
    pub async fn connect_from_configs(cwd: &Path) -> Self {
        let configs = match load_mcp_configs(cwd) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };

        let registry: McpServerRegistry = Arc::new(tokio::sync::RwLock::new(Vec::new()));
        let auth_store = Arc::new(McpAuthStore::new());
        let resource_tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(ListMcpResourcesTool::new(registry.clone())),
            Arc::new(ListMcpResourceTemplatesTool::new(registry.clone())),
            Arc::new(ReadMcpResourceTool::new(registry.clone())),
        ];

        let mut mgr = Self {
            cwd: cwd.to_path_buf(),
            configs: configs.clone(),
            clients: Vec::new(),
            tools: Vec::new(),
            resource_tools,
            summaries: Vec::new(),
            registry,
            auth_store,
        };

        let mut sorted_names: Vec<String> = configs.keys().cloned().collect();
        sorted_names.sort();

        let mut server_tools: Vec<Arc<dyn AgentTool>> = Vec::new();

        for name in &sorted_names {
            let cfg = match configs.get(name) {
                Some(c) => c,
                None => continue,
            };

            if cfg.enabled == Some(false) {
                continue;
            }

            match Self::connect_server_internal(name, cfg, cwd, &mgr.auth_store).await {
                Ok((client, tool_infos)) => {
                    let tool_count = tool_infos.len();
                    for tool_info in tool_infos {
                        server_tools.push(Arc::new(McpAgentTool::new(
                            name.clone(),
                            tool_info,
                            client.clone(),
                        )));
                    }
                    mgr.clients.push((name.clone(), client));
                    mgr.summaries.push(McpConnectionSummary {
                        server: name.clone(),
                        tool_count,
                        error: None,
                    });
                }
                Err(e) => {
                    mgr.summaries.push(McpConnectionSummary {
                        server: name.clone(),
                        tool_count: 0,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        // Sync registry with all connected clients
        *mgr.registry.write().await = mgr.clients.clone();

        // Include resource tools if at least one server is configured or connected
        if !mgr.configs.is_empty() || !mgr.clients.is_empty() {
            server_tools.extend(mgr.resource_tools.clone());
        }
        mgr.tools = server_tools;

        mgr
    }

    async fn create_transport_for_server(
        name: &str,
        cfg: &McpServerConfig,
        cwd: &Path,
        auth_store: &Arc<McpAuthStore>,
    ) -> anyhow::Result<Arc<dyn McpTransport>> {
        if let Some(ref cmd) = cfg.command {
            let cwd_ref = cfg.cwd.as_deref().or(Some(cwd));
            StdioTransport::spawn(cmd, &cfg.args, &cfg.env, cwd_ref)
                .map(|t| Arc::new(t) as Arc<dyn McpTransport>)
        } else if let Some(ref url) = cfg.url {
            let needs_oauth = cfg.auth.as_deref() == Some("oauth")
                || auth_store
                    .get(name, url)
                    .await
                    .is_some_and(|e| e.tokens.is_some());

            if needs_oauth {
                let bearer_provider = Arc::new(StoredMcpBearerProvider::new(
                    auth_store.clone(),
                    name,
                    url,
                    cfg.oauth_token_endpoint.clone(),
                ));
                let http_transport = StreamableHttpTransport::new(url.clone(), cfg.headers.clone())
                    .with_auth_provider(bearer_provider);
                Ok(Arc::new(http_transport))
            } else {
                Ok(Arc::new(StreamableHttpTransport::new(
                    url.clone(),
                    cfg.headers.clone(),
                )))
            }
        } else {
            anyhow::bail!("Neither command nor url specified for MCP server '{name}'")
        }
    }

    async fn connect_server_internal(
        name: &str,
        cfg: &McpServerConfig,
        cwd: &Path,
        auth_store: &Arc<McpAuthStore>,
    ) -> anyhow::Result<(Arc<McpClient>, Vec<McpToolInfo>)> {
        let transport = Self::create_transport_for_server(name, cfg, cwd, auth_store).await?;
        let root_name = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| "workspace".to_string());
        let roots = vec![McpRoot {
            uri: cwd_to_root_uri(cwd),
            name: Some(root_name),
        }];

        let timeout_duration = cfg
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(15));

        let connect_and_list = async {
            let client = Arc::new(McpClient::connect_with_roots(transport, roots).await?);
            let tool_infos = client.list_tools().await?;
            Ok::<_, anyhow::Error>((client, tool_infos))
        };

        tokio::time::timeout(timeout_duration, connect_and_list)
            .await
            .map_err(|_| anyhow::anyhow!("Connection timed out after {:?}", timeout_duration))?
    }

    /// Returns the active tools provided by connected MCP servers and resource meta-tools.
    pub fn tools(&self) -> &[Arc<dyn AgentTool>] {
        &self.tools
    }

    /// Returns the 3 resource meta-tools (list_mcp_resources, list_mcp_resource_templates, read_mcp_resource).
    pub fn resource_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.resource_tools.clone()
    }

    /// Returns the connection status summaries for all configured MCP servers.
    pub fn summaries(&self) -> &[McpConnectionSummary] {
        &self.summaries
    }

    /// Returns the synchronized MCP server registry.
    pub fn registry(&self) -> McpServerRegistry {
        self.registry.clone()
    }

    /// Returns the shared persistent authentication store.
    pub fn auth_store(&self) -> Arc<McpAuthStore> {
        self.auth_store.clone()
    }

    /// Returns the list of configured server names in alphabetical order.
    pub fn configured_server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.configs.keys().cloned().collect();
        names.sort();
        names
    }

    /// Reconnects a specific server by name, updating active tools and registry.
    pub async fn reconnect_server(&mut self, name: &str) -> anyhow::Result<McpConnectionSummary> {
        let cfg = self
            .configs
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Server '{name}' is not configured"))?;

        // Close and remove existing client for this server
        if let Some(pos) = self.clients.iter().position(|(n, _)| n == name) {
            let (_, client) = self.clients.remove(pos);
            let _ = client.close().await;
        }

        // Remove old server tools (tools starting with mcp__{name}__)
        let prefix = format!("mcp__{}__", name);
        self.tools.retain(|t| !t.name().starts_with(&prefix));

        // Remove old summary
        self.summaries.retain(|s| s.server != name);

        let summary = if cfg.enabled == Some(false) {
            McpConnectionSummary {
                server: name.to_string(),
                tool_count: 0,
                error: Some("Server is disabled".to_string()),
            }
        } else {
            match Self::connect_server_internal(name, &cfg, &self.cwd, &self.auth_store).await {
                Ok((client, tool_infos)) => {
                    let tool_count = tool_infos.len();
                    for tool_info in tool_infos {
                        self.tools.push(Arc::new(McpAgentTool::new(
                            name.to_string(),
                            tool_info,
                            client.clone(),
                        )));
                    }
                    self.clients.push((name.to_string(), client));
                    McpConnectionSummary {
                        server: name.to_string(),
                        tool_count,
                        error: None,
                    }
                }
                Err(e) => McpConnectionSummary {
                    server: name.to_string(),
                    tool_count: 0,
                    error: Some(e.to_string()),
                },
            }
        };

        self.summaries.push(summary.clone());
        *self.registry.write().await = self.clients.clone();

        // Ensure resource tools are present if servers exist
        if !self.configs.is_empty() || !self.clients.is_empty() {
            for res_tool in &self.resource_tools {
                if !self.tools.iter().any(|t| t.name() == res_tool.name()) {
                    self.tools.push(res_tool.clone());
                }
            }
        }

        Ok(summary)
    }

    /// Reconnects all configured servers.
    pub async fn reconnect_all(&mut self) -> Vec<McpConnectionSummary> {
        let names = self.configured_server_names();
        let mut results = Vec::new();
        for name in names {
            if let Ok(summary) = self.reconnect_server(&name).await {
                results.push(summary);
            }
        }
        results
    }

    /// Performs OAuth 2.0 PKCE authentication flow for an HTTP MCP server.
    pub async fn authenticate_server_with_prompt<F>(
        &mut self,
        name: &str,
        on_url: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(&str),
    {
        let cfg = self
            .configs
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Server '{name}' is not configured"))?;
        let url = cfg
            .url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Server '{name}' is not an HTTP server"))?;

        let http_client = reqwest::Client::new();
        let meta = mehh_mcp::oauth::discover_oauth_metadata(&http_client, url).await?;

        // Bind an ephemeral loopback port for the OAuth callback
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind ephemeral loopback listener")?;
        let port = listener
            .local_addr()
            .context("Failed to get local port")?
            .port();
        drop(listener);

        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let existing_entry = self.auth_store.get(name, url).await;
        let (client_id, client_secret) = if let Some(ref entry) = existing_entry {
            if let Some(ref cid) = entry.client_id {
                (cid.clone(), entry.client_secret.clone())
            } else if meta.registration_endpoint.is_some() {
                mehh_mcp::oauth::register_dynamic_client(
                    &http_client,
                    &meta,
                    &redirect_uri,
                    "mehh-agent",
                )
                .await?
            } else {
                ("dynamic_client".to_string(), None)
            }
        } else if meta.registration_endpoint.is_some() {
            mehh_mcp::oauth::register_dynamic_client(
                &http_client,
                &meta,
                &redirect_uri,
                "mehh-agent",
            )
            .await?
        } else {
            ("dynamic_client".to_string(), None)
        };

        let (verifier, challenge) = mehh_mcp::oauth::generate_mcp_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
            meta.authorization_endpoint,
            mehh_ai::percent_encode(&client_id),
            mehh_ai::percent_encode(&redirect_uri),
            mehh_ai::percent_encode(&challenge),
            mehh_ai::percent_encode(&state),
        );

        on_url(&auth_url);

        let auth_code = mehh_mcp::oauth::run_loopback_callback(
            port,
            &state,
            Duration::from_secs(120),
        )
        .await?;

        let mut form_params: HashMap<&str, &str> = HashMap::new();
        form_params.insert("grant_type", "authorization_code");
        form_params.insert("code", &auth_code);
        form_params.insert("redirect_uri", &redirect_uri);
        form_params.insert("client_id", &client_id);
        form_params.insert("code_verifier", &verifier);
        if let Some(ref secret) = client_secret {
            form_params.insert("client_secret", secret);
        }

        let token_resp = http_client
            .post(&meta.token_endpoint)
            .form(&form_params)
            .send()
            .await
            .context("Failed to POST token request to authorization server")?
            .error_for_status()
            .context("Token endpoint returned error status")?;

        let tokens: OAuthTokens = token_resp
            .json()
            .await
            .context("Failed to parse token response JSON")?;

        let expires_at = tokens
            .expires_in
            .map(|exp| mehh_ai::now_ms() + (exp as i64 * 1000));

        let auth_entry = McpAuthEntry {
            server_url: url.clone(),
            client_id: Some(client_id),
            client_secret,
            tokens: Some(tokens),
            tokens_expire_at: expires_at,
        };

        self.auth_store.set(name, url, auth_entry).await?;
        self.reconnect_server(name).await?;

        Ok(())
    }

    /// Gracefully closes all active MCP client connections.
    pub async fn close_all(&self) {
        for (_, client) in &self.clients {
            let _ = client.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mcp_manager_empty_configs() {
        let temp_dir =
            std::env::temp_dir().join(format!("mehh_mcp_mgr_test_{}", uuid::Uuid::now_v7()));
        let mgr = McpManager::connect_from_configs(&temp_dir).await;
        assert!(mgr.tools().is_empty());
        assert!(mgr.summaries().is_empty());
        assert!(mgr.configured_server_names().is_empty());
        mgr.close_all().await;
    }

    #[test]
    fn test_cwd_to_root_uri() {
        let p = Path::new("C:\\Users\\test\\project");
        let uri = cwd_to_root_uri(p);
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("Users/test/project") || uri.contains("project"));

        let unix_p = Path::new("/var/project");
        let unix_uri = cwd_to_root_uri(unix_p);
        assert!(unix_uri.starts_with("file://"));
    }

    #[tokio::test]
    async fn test_resource_tools_and_registry_access() {
        let mgr = McpManager::default();
        let res_tools = mgr.resource_tools();
        assert_eq!(res_tools.len(), 3);
        let names: Vec<&str> = res_tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"list_mcp_resources"));
        assert!(names.contains(&"list_mcp_resource_templates"));
        assert!(names.contains(&"read_mcp_resource"));

        let reg = mgr.registry();
        assert!(reg.read().await.is_empty());
    }
}
