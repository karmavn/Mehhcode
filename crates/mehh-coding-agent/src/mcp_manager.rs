//! MCP server connection management, tool lifecycle, and summaries.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use mehh_agent::AgentTool;
use mehh_mcp::{
    create_mcp_agent_tools_with_shared_names_and_timeout, load_mcp_configs_with_trust,
    ListMcpResourceTemplatesTool, ListMcpResourcesTool, McpAuthEntry, McpAuthStore, McpClient,
    McpRoot, McpServerConfig, McpServerRegistry, McpToolInfo, McpTransport, OAuthTokens,
    ReadMcpResourceTool, StdioTransport, StoredMcpBearerProvider, StreamableHttpTransport,
};

const MAX_OAUTH_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_OAUTH_ERROR_BYTES: usize = 16 * 1024;
const OAUTH_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection status summary for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConnectionSummary {
    pub server: String,
    pub tool_count: usize,
    pub error: Option<String>,
}

/// Report returned by [`McpManager::refresh_server_tools_report`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpRefreshReport {
    pub changed: bool,
    pub errors: Vec<String>,
}

fn is_method_not_found_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.starts_with("MCP error -32601:") || msg.starts_with("MCP error -32601 ")
}

async fn list_tools_if_supported(client: &McpClient) -> anyhow::Result<Vec<McpToolInfo>> {
    let supports_tools = client
        .server_capabilities()
        .await
        .and_then(|caps| caps.get("tools").cloned())
        .is_some_and(|v| !v.is_null());
    if !supports_tools {
        return Ok(Vec::new());
    }
    match client.list_tools().await {
        Ok(tools) => Ok(tools),
        Err(err) if is_method_not_found_error(&err) => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

fn redact_token_error_body(raw: &str, secrets: &[&str]) -> String {
    let mut s = raw.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            s = s.replace(secret, "[REDACTED]");
        }
    }
    s.chars().take(512).collect()
}

/// Converts a local filesystem path (including Windows verbatim `\\?\` and UNC paths)
/// to a standard `file://` URI.
pub fn cwd_to_root_uri(cwd: &Path) -> String {
    let abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let raw = abs.to_string_lossy();
    let stripped = crate::settings::strip_windows_verbatim_prefix(&raw);
    let path_str = stripped.replace('\\', "/");

    if let Some(unc_rest) = path_str.strip_prefix("//") {
        let encoded = unc_rest
            .split('/')
            .map(mehh_ai::percent_encode)
            .collect::<Vec<_>>()
            .join("/");
        return format!("file://{encoded}");
    }

    let encoded = path_str
        .split('/')
        .enumerate()
        .map(|(i, segment)| {
            if i == 0 && segment.len() == 2 && segment.as_bytes()[1] == b':' {
                format!("{}:", mehh_ai::percent_encode(&segment[..1]))
            } else {
                mehh_ai::percent_encode(segment)
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn build_oauth_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(authorization_endpoint)
        .with_context(|| format!("Invalid OAuth authorization endpoint '{authorization_endpoint}'"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.into())
}

async fn read_bounded_oauth_json_bytes(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            anyhow::bail!(
                "OAuth token response Content-Length ({len} bytes) exceeds limit of {max_bytes} bytes"
            );
        }
    }
    let mut buf = Vec::new();
    while let Some(chunk) = tokio::time::timeout(OAUTH_BODY_READ_TIMEOUT, resp.chunk())
        .await
        .map_err(|_| anyhow::anyhow!("Timed out reading OAuth token response body"))??
    {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("OAuth token response body exceeded limit of {max_bytes} bytes");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Manages MCP client connections, active MCP tools, resource meta-tools, and OAuth storage.
pub struct McpManager {
    cwd: PathBuf,
    configs: HashMap<String, McpServerConfig>,
    clients: Vec<(String, Arc<McpClient>)>,
    server_tools: HashMap<String, Vec<Arc<dyn AgentTool>>>,
    tools: Vec<Arc<dyn AgentTool>>,
    resource_tools: Vec<Arc<dyn AgentTool>>,
    summaries: Vec<McpConnectionSummary>,
    registry: McpServerRegistry,
    auth_store: Arc<McpAuthStore>,
    tools_dirty: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for McpManager {
    fn default() -> Self {
        let registry: McpServerRegistry = Arc::new(tokio::sync::RwLock::new(Vec::new()));
        let auth_store = Arc::new(McpAuthStore::try_new().unwrap_or_else(|_| {
            let fallback = std::env::temp_dir().join(format!(
                "mehh-mcp-auth-fallback-{}.json",
                std::process::id()
            ));
            McpAuthStore::try_with_path(fallback)
                .expect("temp-dir MCP auth store fallback must initialize")
        }));
        let resource_tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(ListMcpResourcesTool::new(registry.clone())),
            Arc::new(ListMcpResourceTemplatesTool::new(registry.clone())),
            Arc::new(ReadMcpResourceTool::new(registry.clone())),
        ];
        Self {
            cwd: PathBuf::new(),
            configs: HashMap::new(),
            clients: Vec::new(),
            server_tools: HashMap::new(),
            tools: Vec::new(),
            resource_tools,
            summaries: Vec::new(),
            registry,
            auth_store,
            tools_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl McpManager {
    /// Loads MCP configs from disk (`~/.mehh/mcp.json` and, only when `trusted` is true,
    /// `<cwd>/.mehh/mcp.json`) and attempts connection to each enabled server.
    /// Fails closed on malformed config JSON or credential store errors.
    pub async fn connect_from_configs_with_trust(
        cwd: &Path,
        trusted: bool,
    ) -> anyhow::Result<Self> {
        let configs = load_mcp_configs_with_trust(cwd, trusted)?;
        let auth_store = Arc::new(McpAuthStore::try_new()?);
        Ok(Self::connect_with_configs(cwd, configs, auth_store).await)
    }

    /// Connects to an already-loaded map of MCP server configs using the given auth store.
    pub async fn connect_with_configs(
        cwd: &Path,
        configs: HashMap<String, McpServerConfig>,
        auth_store: Arc<McpAuthStore>,
    ) -> Self {
        let registry: McpServerRegistry = Arc::new(tokio::sync::RwLock::new(Vec::new()));
        let resource_tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(ListMcpResourcesTool::new(registry.clone())),
            Arc::new(ListMcpResourceTemplatesTool::new(registry.clone())),
            Arc::new(ReadMcpResourceTool::new(registry.clone())),
        ];

        let mut mgr = Self {
            cwd: cwd.to_path_buf(),
            configs: configs.clone(),
            clients: Vec::new(),
            server_tools: HashMap::new(),
            tools: Vec::new(),
            resource_tools,
            summaries: Vec::new(),
            registry,
            auth_store,
            tools_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let mut sorted_names: Vec<String> = configs.keys().cloned().collect();
        sorted_names.sort();

        let enabled: Vec<(&String, &McpServerConfig)> = sorted_names
            .iter()
            .filter_map(|name| {
                let cfg = configs.get(name)?;
                if cfg.enabled == Some(false) {
                    None
                } else {
                    Some((name, cfg))
                }
            })
            .collect();

        let connect_futs = enabled.iter().map(|(name, cfg)| {
            let auth_store = &mgr.auth_store;
            async move {
                let res = Self::connect_server_internal(name, cfg, cwd, auth_store).await;
                ((*name).clone(), res)
            }
        });
        let results = futures::future::join_all(connect_futs).await;

        let mut used_names: HashSet<String> = HashSet::new();
        for (name, res) in results {
            match res {
                Ok((client, tool_infos)) => {
                    let dirty = mgr.tools_dirty.clone();
                    client.on_notification(
                        "notifications/tools/list_changed",
                        Arc::new(move |_params| {
                            dirty.store(true, std::sync::atomic::Ordering::SeqCst);
                        }),
                    );
                    let timeout = mgr
                        .configs
                        .get(&name)
                        .and_then(|c| c.effective_timeout_ms())
                        .map(Duration::from_millis);
                    let adapted = create_mcp_agent_tools_with_shared_names_and_timeout(
                        &name,
                        tool_infos,
                        client.clone(),
                        &mut used_names,
                        timeout,
                    );
                    let tool_count = adapted.len();
                    mgr.server_tools.insert(name.clone(), adapted);
                    mgr.clients.push((name.clone(), client));
                    mgr.summaries.push(McpConnectionSummary {
                        server: name,
                        tool_count,
                        error: None,
                    });
                }
                Err(e) => {
                    mgr.summaries.push(McpConnectionSummary {
                        server: name,
                        tool_count: 0,
                        error: Some(e.to_string()),
                    });
                }
            }
        }

        // Sync registry with all connected clients
        *mgr.registry.write().await = mgr.clients.clone();
        mgr.rebuild_tools();

        mgr
    }

    /// Names already used by other servers, so a sanitized collision
    /// (`svc-a` and `svc_a` both become `mcp__svc_a__*`) cannot make two tools
    /// share a name. Dispatch by name would otherwise reach the wrong server.
    fn names_used_by_other_servers(&self, except: Option<&str>) -> HashSet<String> {
        self.server_tools
            .iter()
            .filter(|(name, _)| except != Some(name.as_str()))
            .flat_map(|(_, tools)| tools.iter().map(|tool| tool.name().to_string()))
            .collect()
    }

    fn rebuild_tools(&mut self) {
        let mut rebuilt = Vec::new();
        let mut names: Vec<&String> = self.server_tools.keys().collect();
        names.sort();
        for name in names {
            if let Some(tools) = self.server_tools.get(name) {
                rebuilt.extend(tools.iter().cloned());
            }
        }
        if !self.configs.is_empty() || !self.clients.is_empty() {
            rebuilt.extend(self.resource_tools.iter().cloned());
        }
        self.tools = rebuilt;
    }

    async fn create_transport_for_server(
        name: &str,
        cfg: &McpServerConfig,
        cwd: &Path,
        auth_store: &Arc<McpAuthStore>,
    ) -> anyhow::Result<Arc<dyn McpTransport>> {
        if let Some(ref cmd) = cfg.command {
            let cwd_ref = cfg.cwd.as_deref().or(Some(cwd));
            StdioTransport::spawn(cmd, cfg.args(), &cfg.env, cwd_ref)
                .map(|t| Arc::new(t) as Arc<dyn McpTransport>)
        } else if let Some(ref url) = cfg.url {
            let needs_oauth = cfg.auth.as_deref() == Some("oauth")
                || auth_store
                    .get(name, url)
                    .await
                    .is_some_and(|e| e.tokens.is_some());

            let mut http_transport = StreamableHttpTransport::new(url.clone(), cfg.headers.clone());
            if let Some(ms) = cfg.effective_timeout_ms() {
                http_transport = http_transport.with_request_timeout(Duration::from_millis(ms));
            }
            if needs_oauth {
                let bearer_provider = Arc::new(StoredMcpBearerProvider::new(
                    auth_store.clone(),
                    name,
                    url,
                    cfg.oauth_token_endpoint.clone(),
                )?);
                http_transport = http_transport.with_auth_provider(bearer_provider);
            }
            Ok(Arc::new(http_transport))
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
            .effective_timeout_ms()
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(15));

        let connect_and_list = async {
            let client = Arc::new(
                McpClient::connect_with_roots_and_timeout(transport, roots, timeout_duration)
                    .await?,
            );
            let tool_infos = list_tools_if_supported(&client).await?;
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

    /// Returns true and resets the dirty flag if any server reported `notifications/tools/list_changed`.
    pub fn take_tools_dirty(&self) -> bool {
        self.tools_dirty
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Explicitly marks the tools dirty flag.
    pub fn mark_tools_dirty(&self) {
        self.tools_dirty
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fetches the latest tools from all connected MCP clients and returns a structured report.
    pub async fn refresh_server_tools_report(&mut self) -> McpRefreshReport {
        let mut changed = false;
        let mut errors = Vec::new();
        for (name, client) in &self.clients {
            match list_tools_if_supported(client).await {
                Ok(new_tools) => {
                    let mut used = self.names_used_by_other_servers(Some(name));
                    let timeout = self
                        .configs
                        .get(name)
                        .and_then(|c| c.effective_timeout_ms())
                        .map(Duration::from_millis);
                    let adapted = create_mcp_agent_tools_with_shared_names_and_timeout(
                        name,
                        new_tools,
                        client.clone(),
                        &mut used,
                        timeout,
                    );
                    self.server_tools.insert(name.clone(), adapted);
                    changed = true;
                }
                Err(err) => {
                    errors.push(format!("{name}: {err:#}"));
                }
            }
        }
        if changed {
            self.rebuild_tools();
            for summary in &mut self.summaries {
                if let Some(tools) = self.server_tools.get(&summary.server) {
                    summary.tool_count = tools.len();
                }
            }
        }
        McpRefreshReport { changed, errors }
    }

    /// Fetches the latest tools from all connected MCP clients and updates active tools.
    pub async fn refresh_server_tools(&mut self) -> anyhow::Result<bool> {
        let report = self.refresh_server_tools_report().await;
        if !report.errors.is_empty() {
            anyhow::bail!(
                "Failed to refresh tools for MCP server(s): {}",
                report.errors.join("; ")
            );
        }
        Ok(report.changed)
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

        // Remove all prior tools belonging to this configured server (handles truncated >=64-byte names)
        self.server_tools.remove(name);

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
                    let dirty = self.tools_dirty.clone();
                    client.on_notification(
                        "notifications/tools/list_changed",
                        Arc::new(move |_params| {
                            dirty.store(true, std::sync::atomic::Ordering::SeqCst);
                        }),
                    );
                    let mut used = self.names_used_by_other_servers(Some(name));
                    let timeout = cfg.effective_timeout_ms().map(Duration::from_millis);
                    let adapted = create_mcp_agent_tools_with_shared_names_and_timeout(
                        name,
                        tool_infos,
                        client.clone(),
                        &mut used,
                        timeout,
                    );
                    let tool_count = adapted.len();
                    self.server_tools.insert(name.to_string(), adapted);
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
        self.rebuild_tools();

        Ok(summary)
    }

    /// Reconnects all configured servers.
    pub async fn reconnect_all(&mut self) -> Vec<McpConnectionSummary> {
        let names = self.configured_server_names();
        let mut results = Vec::new();
        for name in names {
            match self.reconnect_server(&name).await {
                Ok(summary) => results.push(summary),
                Err(err) => {
                    results.push(McpConnectionSummary {
                        server: name,
                        tool_count: 0,
                        error: Some(err.to_string()),
                    });
                }
            }
        }
        results
    }

    /// Extracts the server URL and shared auth store for background OAuth login.
    pub fn prepare_server_oauth(&self, name: &str) -> anyhow::Result<(String, Arc<McpAuthStore>)> {
        let cfg = self
            .configs
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Server '{name}' is not configured"))?;
        let url = cfg
            .url
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Server '{name}' is not an HTTP server"))?;
        Ok((url.clone(), self.auth_store.clone()))
    }

    /// Runs OAuth 2.0 PKCE discovery, loopback callback, and token exchange against `auth_store`.
    pub async fn complete_server_oauth_flow<F>(
        name: &str,
        url: &str,
        auth_store: &McpAuthStore,
        on_url: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(&str),
    {
        let http_client = mehh_mcp::oauth::oauth_http_client()?;
        let meta = mehh_mcp::oauth::discover_oauth_metadata(&http_client, url).await?;

        // Bind an ephemeral loopback port for the OAuth callback
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind ephemeral loopback listener")?;
        let port = listener
            .local_addr()
            .context("Failed to get local port")?
            .port();
        // The listener stays bound: the callback helper takes ownership so no
        // other process can claim the port between URL printing and accept().

        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let existing_entry = auth_store.try_get(name, url).await?;
        let (client_id, client_secret) = if meta.registration_endpoint.is_some() {
            mehh_mcp::oauth::register_dynamic_client(
                &http_client,
                &meta,
                &redirect_uri,
                "mehh-agent",
            )
            .await?
        } else if let Some(cid) = existing_entry.as_ref().and_then(|e| e.client_id.as_ref()) {
            (
                cid.clone(),
                existing_entry.as_ref().and_then(|e| e.client_secret.clone()),
            )
        } else {
            ("dynamic_client".to_string(), None)
        };

        let (verifier, challenge) = mehh_mcp::oauth::generate_mcp_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let auth_url = build_oauth_authorize_url(
            &meta.authorization_endpoint,
            &client_id,
            &redirect_uri,
            &challenge,
            &state,
        )?;

        on_url(&auth_url);

        let auth_code =
            mehh_mcp::oauth::run_loopback_callback(listener, &state, Duration::from_secs(120))
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

        mehh_mcp::oauth::validate_oauth_request_url(&meta.token_endpoint).await?;
        let token_resp = http_client
            .post(&meta.token_endpoint)
            .header("Accept", "application/json")
            .form(&form_params)
            .send()
            .await
            .context("Failed to POST token request to authorization server")?;

        let status = token_resp.status();
        if !status.is_success() {
            let err_bytes = read_bounded_oauth_json_bytes(token_resp, MAX_OAUTH_ERROR_BYTES)
                .await
                .unwrap_or_default();
            let err_body = String::from_utf8_lossy(&err_bytes);
            let mut secrets: Vec<&str> = vec![auth_code.as_str(), verifier.as_str()];
            if let Some(ref secret) = client_secret {
                secrets.push(secret.as_str());
            }
            let safe_body = redact_token_error_body(&err_body, &secrets);
            anyhow::bail!("Token endpoint returned error status ({status}): {safe_body}");
        }

        let body_bytes = read_bounded_oauth_json_bytes(token_resp, MAX_OAUTH_RESPONSE_BYTES).await?;
        let tokens: OAuthTokens = serde_json::from_slice(&body_bytes)
            .context("Failed to parse token response JSON")?;

        // tokens_expire_at is Unix SECONDS, not milliseconds: it is compared
        // against SystemTime::as_secs() in StoredMcpBearerProvider.
        let expires_at = tokens
            .expires_in
            .map(mehh_mcp::oauth::mcp_token_expiry_seconds);

        let auth_entry = McpAuthEntry {
            server_url: url.to_string(),
            client_id: Some(client_id),
            client_secret,
            tokens: Some(tokens),
            tokens_expire_at: expires_at,
        };

        auth_store.set(name, url, auth_entry).await?;
        Ok(())
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
        let (url, auth_store) = self.prepare_server_oauth(name)?;
        Self::complete_server_oauth_flow(name, &url, &auth_store, on_url).await?;
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
    use mehh_mcp::create_mcp_agent_tools;

    #[tokio::test]
    async fn test_mcp_manager_empty_configs() {
        let temp_dir = crate::TempTestDir::new("mcp_mgr");
        let store =
            Arc::new(McpAuthStore::try_with_path(temp_dir.path().join("mcp-auth.json")).unwrap());
        let configs = HashMap::new();
        let mgr = McpManager::connect_with_configs(temp_dir.path(), configs, store).await;
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

    #[test]
    fn test_cwd_to_root_uri_existing_dir_has_no_encoded_extended_prefix() {
        // Use a real directory so canonicalize() succeeds. On Windows the canonical
        // path carries the \\?\ extended-length prefix; it must be stripped before
        // percent-encoding, and the drive-letter colon must remain unescaped.
        let dir = std::env::temp_dir();
        let uri = cwd_to_root_uri(&dir);
        assert!(uri.starts_with("file:///"), "unexpected uri: {uri}");
        assert!(
            !uri.contains("%3F"),
            "extended-length '?' leaked into uri: {uri}"
        );
        assert!(
            !uri.contains("%3A"),
            "drive-letter colon was percent-encoded: {uri}"
        );
        assert!(
            !uri.contains("//%"),
            "double-encoded prefix leaked into uri: {uri}"
        );
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

    #[tokio::test]
    async fn test_connect_from_configs_with_trust_fails_closed_on_corrupt_trusted_project_config() {
        let dir = crate::TempTestDir::new("mcp_mgr_corrupt");
        let mehh_dir = dir.path().join(".mehh");
        std::fs::create_dir_all(&mehh_dir).unwrap();
        std::fs::write(mehh_dir.join("mcp.json"), b"{invalid-json").unwrap();

        let err = McpManager::connect_from_configs_with_trust(dir.path(), true)
            .await
            .err()
            .expect("trusted corrupt project mcp.json must fail closed");
        let msg = format!("{err:#}");
        assert!(msg.contains("mcp.json"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn tools_list_changed_refreshes_catalog_and_codemode() {
        use mehh_mcp::jsonrpc::{JsonRpcMessage, JsonRpcNotification, JsonRpcResponse};
        use mehh_mcp::transport::{InMemoryTransport, McpTransport};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (client_transport, server_transport) = InMemoryTransport::pair();
        let client_transport = Arc::new(client_transport);
        let server_transport = Arc::new(server_transport);

        let tool_query_count = Arc::new(AtomicUsize::new(0));
        let count_clone = tool_query_count.clone();

        tokio::spawn({
            let server_transport = server_transport.clone();
            async move {
                // Handshake
                if let Some(Ok(JsonRpcMessage::Request(req))) = server_transport.recv().await {
                    let resp = JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "tools": { "listChanged": true } },
                            "serverInfo": { "name": "mock-server", "version": "1.0.0" }
                        }),
                    );
                    server_transport
                        .send(JsonRpcMessage::Response(resp))
                        .await
                        .unwrap();
                }
                let _ = server_transport.recv().await; // notifications/initialized

                while let Some(Ok(msg)) = server_transport.recv().await {
                    match msg {
                        JsonRpcMessage::Request(req) if req.method == "tools/list" => {
                            let c = count_clone.fetch_add(1, Ordering::SeqCst);
                            let tools = if c == 0 {
                                vec![serde_json::json!({
                                    "name": "initial_tool",
                                    "description": "Initial tool",
                                    "inputSchema": { "type": "object" }
                                })]
                            } else {
                                vec![
                                    serde_json::json!({
                                        "name": "initial_tool",
                                        "description": "Initial tool",
                                        "inputSchema": { "type": "object" }
                                    }),
                                    serde_json::json!({
                                        "name": "new_tool",
                                        "description": "Newly added dynamic tool",
                                        "inputSchema": { "type": "object" }
                                    }),
                                ]
                            };
                            let resp = JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({ "tools": tools }),
                            );
                            server_transport
                                .send(JsonRpcMessage::Response(resp))
                                .await
                                .unwrap();
                        }
                        _ => {}
                    }
                }
            }
        });

        let client = Arc::new(McpClient::connect(client_transport).await.unwrap());
        let tools = client.list_tools().await.unwrap();
        let adapted = create_mcp_agent_tools("mock", tools, client.clone());

        let mut mgr = McpManager::default();
        let dirty = mgr.tools_dirty.clone();
        client.on_notification(
            "notifications/tools/list_changed",
            Arc::new(move |_params| {
                dirty.store(true, Ordering::SeqCst);
            }),
        );
        mgr.clients.push(("mock".to_string(), client.clone()));
        mgr.server_tools.insert("mock".to_string(), adapted);
        mgr.rebuild_tools();

        // Create AgentSession
        let temp_dir = crate::TempTestDir::new("mcp_sync_test");
        let model = mehh_ai::Model {
            id: "faux-1".to_string(),
            name: "Faux".to_string(),
            api: "faux".to_string(),
            provider: "faux".to_string(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".to_string()],
            cost: mehh_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
            thinking_level_map: None,
        };
        let provider = Arc::new(mehh_ai::FauxProvider::new());
        let session_mgr = crate::session::SessionManager::in_memory(temp_dir.path().to_path_buf());
        let settings = crate::settings::Settings::default();
        let mut session = crate::agent_session::AgentSession::new(
            temp_dir.path().to_path_buf(),
            model,
            provider,
            session_mgr,
            settings,
        );

        session.enable_mcp_and_codemode(mgr.tools().to_vec(), true);
        assert!(!mgr.take_tools_dirty());
        let codemode = session.codemode_tool().expect("codemode should be enabled");
        assert!(codemode
            .tools()
            .iter()
            .any(|t| t.name() == "mcp__mock__initial_tool"));
        assert!(!codemode
            .tools()
            .iter()
            .any(|t| t.name() == "mcp__mock__new_tool"));

        // Now simulate server notifying tools/list_changed
        server_transport
            .send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                "notifications/tools/list_changed",
                None,
            )))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            mgr.take_tools_dirty(),
            "dirty flag must be true after notification"
        );

        let refreshed = mgr.refresh_server_tools().await.unwrap();
        assert!(refreshed);

        session.sync_mcp_tools(mgr.tools().to_vec(), true);
        let updated_codemode = session
            .codemode_tool()
            .expect("codemode should still be present");
        assert!(updated_codemode
            .tools()
            .iter()
            .any(|t| t.name() == "mcp__mock__initial_tool"));
        assert!(updated_codemode
            .tools()
            .iter()
            .any(|t| t.name() == "mcp__mock__new_tool"));

        mgr.close_all().await;
    }

    #[test]
    fn cwd_to_root_uri_handles_unc_and_verbatim_unc_paths() {
        assert_eq!(
            cwd_to_root_uri(Path::new(r"\\?\UNC\fileserver\share\my repo")),
            "file://fileserver/share/my%20repo"
        );
        assert_eq!(
            cwd_to_root_uri(Path::new(r"\\?\unc\fileserver\share\proj")),
            "file://fileserver/share/proj"
        );
        assert_eq!(
            cwd_to_root_uri(Path::new(r"\\fileserver\share\proj")),
            "file://fileserver/share/proj"
        );
    }

    #[test]
    fn build_oauth_authorize_url_preserves_existing_query_and_encodes_params() {
        let url = build_oauth_authorize_url(
            "https://auth.example.com/authorize?tenant=acme",
            "client&id=1",
            "http://127.0.0.1:8080/callback",
            "chal+1",
            "st=1",
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("tenant").map(String::as_str), Some("acme"));
        assert_eq!(pairs.get("client_id").map(String::as_str), Some("client&id=1"));
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(pairs.get("state").map(String::as_str), Some("st=1"));
    }

    #[tokio::test]
    async fn read_bounded_oauth_json_bytes_rejects_oversized_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            let body = vec![b'a'; 2048];
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(header.as_bytes()).await;
            let _ = sock.write_all(&body).await;
        });

        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/token"))
            .send()
            .await
            .unwrap();
        let err = read_bounded_oauth_json_bytes(resp, 1024).await.unwrap_err();
        assert!(err.to_string().contains("exceeds limit"));
        let _ = server.await;
    }

    #[tokio::test]
    async fn i19_resource_only_server_without_tools_capability_or_32601_connects_and_refreshes_cleanly() {
        use mehh_mcp::jsonrpc::{JsonRpcMessage, JsonRpcResponse};
        use mehh_mcp::transport::{InMemoryTransport, McpTransport};
        use std::sync::atomic::{AtomicBool, Ordering};

        // 1. Server without "tools" capability must skip tools/list altogether.
        let (c_tr1, s_tr1) = InMemoryTransport::pair();
        let tools_list_called = Arc::new(AtomicBool::new(false));
        let called_flag = tools_list_called.clone();
        let srv1 = tokio::spawn(async move {
            while let Some(Ok(msg)) = s_tr1.recv().await {
                if let JsonRpcMessage::Request(req) = msg {
                    let resp = if req.method == "initialize" {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "resources": {} },
                                "serverInfo": { "name": "res-only", "version": "1.0" }
                            }),
                        )
                    } else if req.method == "tools/list" {
                        called_flag.store(true, Ordering::SeqCst);
                        JsonRpcResponse::error(req.id, -32601, "Method not found", None)
                    } else {
                        JsonRpcResponse::success(req.id, serde_json::json!({}))
                    };
                    let _ = s_tr1.send(JsonRpcMessage::Response(resp)).await;
                }
            }
        });

        let client1 = Arc::new(McpClient::connect(Arc::new(c_tr1)).await.unwrap());

        // 2. Server advertising "tools" capability that returns -32601 must yield empty tool list.
        let (c_tr2, s_tr2) = InMemoryTransport::pair();
        let srv2 = tokio::spawn(async move {
            while let Some(Ok(msg)) = s_tr2.recv().await {
                if let JsonRpcMessage::Request(req) = msg {
                    let resp = if req.method == "initialize" {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "partial-srv", "version": "1.0" }
                            }),
                        )
                    } else if req.method == "tools/list" {
                        JsonRpcResponse::error(req.id, -32601, "Method not found", None)
                    } else {
                        JsonRpcResponse::success(req.id, serde_json::json!({}))
                    };
                    let _ = s_tr2.send(JsonRpcMessage::Response(resp)).await;
                }
            }
        });
        let client2 = Arc::new(McpClient::connect(Arc::new(c_tr2)).await.unwrap());

        let mut mgr = McpManager::default();
        mgr.clients.push(("res_only".to_string(), client1.clone()));
        mgr.clients.push(("partial".to_string(), client2.clone()));
        mgr.refresh_server_tools()
            .await
            .expect("refresh_server_tools must succeed for resource-only and -32601 servers");
        assert!(
            !tools_list_called.load(Ordering::SeqCst),
            "tools/list must be skipped when capabilities lack tools"
        );

        mgr.close_all().await;
        srv1.abort();
        srv2.abort();
    }

    #[tokio::test]
    async fn i21_complete_oauth_flow_fails_closed_on_corrupt_store_reregisters_dcr_and_reads_error_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 1. Corrupt auth store fails closed via try_get.
        let dir = crate::TempTestDir::new("i21_oauth");
        let store_path = dir.path().join("mcp-auth.json");
        let store = McpAuthStore::try_with_path(store_path.clone()).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");

        // Seed store with an old client_id tied to a previous port, and verify DCR re-registers!
        store
            .set(
                "srv",
                &base_url,
                McpAuthEntry {
                    server_url: base_url.clone(),
                    client_id: Some("stale_old_port_client_id".to_string()),
                    client_secret: Some("old_secret".to_string()),
                    tokens: None,
                    tokens_expire_at: None,
                },
            )
            .await
            .unwrap();

        let seen_dcr_redirect = Arc::new(std::sync::Mutex::new(None::<String>));
        let seen_token_accept = Arc::new(std::sync::Mutex::new(None::<String>));
        let seen_token_client_id = Arc::new(std::sync::Mutex::new(None::<String>));
        let dcr_cb = seen_dcr_redirect.clone();
        let accept_cb = seen_token_accept.clone();
        let cid_cb = seen_token_client_id.clone();

        let base_url_srv = base_url.clone();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let dcr_cb = dcr_cb.clone();
                let accept_cb = accept_cb.clone();
                let cid_cb = cid_cb.clone();
                let base_url = base_url_srv.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first_line = req.lines().next().unwrap_or("");
                    let path = first_line.split_whitespace().nth(1).unwrap_or("");

                    let (status, body) = if path.contains("/.well-known/oauth-authorization-server") {
                        let meta = serde_json::json!({
                            "authorization_endpoint": format!("{base_url}/authorize"),
                            "token_endpoint": format!("{base_url}/token"),
                            "registration_endpoint": format!("{base_url}/register"),
                        });
                        ("200 OK", meta.to_string())
                    } else if path == "/register" {
                        let (_, body_part) = req.split_once("\r\n\r\n").unwrap_or(("", ""));
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body_part) {
                            *dcr_cb.lock().unwrap() = v
                                .get("redirect_uris")
                                .and_then(|u| u.get(0))
                                .and_then(|s| s.as_str())
                                .map(str::to_string);
                        }
                        (
                            "200 OK",
                            serde_json::json!({
                                "client_id": "fresh_dcr_client_id",
                                "client_secret": "fresh_dcr_secret"
                            })
                            .to_string(),
                        )
                    } else if path == "/token" {
                        for line in req.lines() {
                            if let Some((k, v)) = line.split_once(':') {
                                if k.eq_ignore_ascii_case("accept") {
                                    *accept_cb.lock().unwrap() = Some(v.trim().to_string());
                                }
                            }
                        }
                        let (_, body_part) = req.split_once("\r\n\r\n").unwrap_or(("", ""));
                        for pair in body_part.split('&') {
                            if let Some((k, v)) = pair.split_once('=') {
                                if k == "client_id" {
                                    *cid_cb.lock().unwrap() = Some(v.to_string());
                                }
                            }
                        }
                        (
                            "400 Bad Request",
                            r#"{"error":"invalid_grant","error_description":"code expired","client_secret":"fresh_dcr_secret"}"#.to_string(),
                        )
                    } else {
                        ("404 Not Found", "{}".to_string())
                    };

                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let flow_res = McpManager::complete_server_oauth_flow(
            "srv",
            &format!("http://127.0.0.1:{port}"),
            &store,
            |auth_url| {
                let parsed = reqwest::Url::parse(auth_url).unwrap();
                let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
                let redir = pairs.get("redirect_uri").unwrap().clone();
                let state = pairs.get("state").unwrap().clone();
                tokio::spawn(async move {
                    let cb_url = format!("{redir}?code=test_code&state={state}");
                    let _ = reqwest::get(cb_url).await;
                });
            },
        )
        .await;

        let err = flow_res.expect_err("400 from token endpoint must fail");
        let err_msg = format!("{err:#}");
        assert!(
            err_msg.contains("invalid_grant") && err_msg.contains("code expired"),
            "token endpoint error body must be surfaced, got: {err_msg}"
        );
        assert!(
            !err_msg.contains("fresh_dcr_secret"),
            "client_secret must be redacted from error, got: {err_msg}"
        );
        assert_eq!(
            seen_token_client_id.lock().unwrap().as_deref(),
            Some("fresh_dcr_client_id"),
            "must register fresh DCR client_id for new ephemeral redirect_uri"
        );
        assert!(
            seen_dcr_redirect.lock().unwrap().is_some(),
            "DCR endpoint must be called with redirect_uri"
        );
        assert_eq!(
            seen_token_accept.lock().unwrap().as_deref(),
            Some("application/json"),
            "token POST must send Accept: application/json"
        );

        // Corrupt the store on disk -> try_get must fail closed after metadata/bind.
        std::fs::write(&store_path, b"{not valid json").unwrap();
        let corrupt_err = McpManager::complete_server_oauth_flow(
            "srv",
            &format!("http://127.0.0.1:{port}"),
            &store,
            |_| {},
        )
        .await
        .expect_err("corrupt auth store must fail closed via try_get");
        assert!(
            format!("{corrupt_err:#}").contains("mcp-auth.json"),
            "expected auth store parse failure, got: {corrupt_err:#}"
        );

        srv.abort();
    }

    #[tokio::test]
    async fn c9_mcp_manager_wires_effective_timeout_ms_into_http_and_tool_calls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req_str = String::from_utf8_lossy(&buf[..n]).to_string();
                    let (_, body) = req_str.split_once("\r\n\r\n").unwrap_or(("", ""));
                    let parsed: serde_json::Value =
                        serde_json::from_str(body.trim()).unwrap_or_default();
                    let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = parsed.get("id").cloned();

                    let resp_body = match method {
                        "initialize" => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "http-srv", "version": "1.0" }
                            }
                        })
                        .to_string(),
                        "notifications/initialized" => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                                .await;
                            return;
                        }
                        "tools/list" => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "slow_tool",
                                    "inputSchema": { "type": "object" }
                                }]
                            }
                        })
                        .to_string(),
                        "tools/call" => {
                            // Delay longer than configured 120ms timeout.
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            return;
                        }
                        _ => String::new(),
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let dir = crate::TempTestDir::new("c9_mgr");
        let store = Arc::new(
            McpAuthStore::try_with_path(dir.path().join("mcp-auth.json")).unwrap(),
        );
        let mut configs = HashMap::new();
        configs.insert(
            "http_srv".to_string(),
            McpServerConfig {
                url: Some(format!("http://127.0.0.1:{port}/mcp")),
                timeout_ms: Some(120),
                ..Default::default()
            },
        );
        let mgr = McpManager::connect_with_configs(dir.path(), configs, store).await;
        assert_eq!(mgr.summaries()[0].error, None);

        let tool = mgr
            .tools()
            .iter()
            .find(|t| t.name() == "mcp__http_srv__slow_tool")
            .expect("tool must be registered");
        let start = std::time::Instant::now();
        let err = tool
            .execute(
                "c1",
                serde_json::Map::new(),
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            err.contains("Timed out") || err.contains("timed out"),
            "expected configured 120ms timeout error, got: {err}"
        );

        mgr.close_all().await;
        srv.abort();
    }

    #[tokio::test]
    async fn i22_refresh_server_tools_report_collects_errors_and_updates_healthy_servers() {
        use mehh_mcp::jsonrpc::{JsonRpcMessage, JsonRpcResponse};
        use mehh_mcp::transport::{InMemoryTransport, McpTransport};

        let (ok_c, ok_s) = InMemoryTransport::pair();
        let ok_srv = tokio::spawn(async move {
            while let Some(Ok(msg)) = ok_s.recv().await {
                if let JsonRpcMessage::Request(req) = msg {
                    let resp = if req.method == "initialize" {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "ok-srv", "version": "1.0" }
                            }),
                        )
                    } else {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "tools": [{
                                    "name": "ping",
                                    "inputSchema": { "type": "object" }
                                }]
                            }),
                        )
                    };
                    let _ = ok_s.send(JsonRpcMessage::Response(resp)).await;
                }
            }
        });

        let (bad_c, bad_s) = InMemoryTransport::pair();
        let bad_srv = tokio::spawn(async move {
            while let Some(Ok(msg)) = bad_s.recv().await {
                if let JsonRpcMessage::Request(req) = msg {
                    let resp = if req.method == "initialize" {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": {} },
                                "serverInfo": { "name": "bad-srv", "version": "1.0" }
                            }),
                        )
                    } else {
                        JsonRpcResponse::error(req.id, -32603, "internal failure", None)
                    };
                    let _ = bad_s.send(JsonRpcMessage::Response(resp)).await;
                }
            }
        });

        let ok_client = Arc::new(McpClient::connect(Arc::new(ok_c)).await.unwrap());
        let bad_client = Arc::new(McpClient::connect(Arc::new(bad_c)).await.unwrap());

        let mut mgr = McpManager::default();
        mgr.clients.push(("ok_srv".to_string(), ok_client));
        mgr.clients.push(("bad_srv".to_string(), bad_client));

        let report = mgr.refresh_server_tools_report().await;
        assert!(report.changed);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("bad_srv"));
        assert!(mgr.tools().iter().any(|t| t.name() == "mcp__ok_srv__ping"));

        let err = mgr.refresh_server_tools().await.unwrap_err();
        assert!(err.to_string().contains("bad_srv"));

        mgr.close_all().await;
        ok_srv.abort();
        bad_srv.abort();
    }
}
