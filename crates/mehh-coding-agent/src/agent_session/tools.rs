use std::sync::Arc;

use mehh_agent::AgentTool;

use super::{subagent_api_key, AgentSession, DEFERRED_MCP_TOOL_THRESHOLD};
use crate::subagent::SubagentTool;
use crate::tool_search::{DeferredToolCatalog, ToolSearchTool};

fn strip_mcp_tools(tools: &mut Vec<Arc<dyn AgentTool>>) {
    tools.retain(|t| {
        !t.name().starts_with("mcp__")
            && t.name() != "list_mcp_resources"
            && t.name() != "list_mcp_resource_templates"
            && t.name() != "read_mcp_resource"
            && t.name() != "tool_search"
            && t.name() != "codemode"
    });
}

impl AgentSession {
    /// Injects MCP tools (deferring large MCP toolsets behind `tool_search` when codemode is off)
    /// and optionally wraps all tools in CodemodeTool, rebuilding the system prompt.
    pub fn enable_mcp_and_codemode(
        &mut self,
        mcp_tools: Vec<Arc<dyn AgentTool>>,
        enable_codemode: bool,
    ) {
        let mcp_server_tool_count = mcp_tools
            .iter()
            .filter(|t| t.name().starts_with("mcp__"))
            .count();
        if !enable_codemode && mcp_server_tool_count > DEFERRED_MCP_TOOL_THRESHOLD {
            self.codemode_tool = None;
            let mut eager = Vec::new();
            let mut deferred = Vec::new();
            for tool in mcp_tools {
                if tool.name().starts_with("mcp__") {
                    deferred.push(tool);
                } else {
                    eager.push(tool);
                }
            }
            if let Some(ref mut custom) = self.subagent_child_tools {
                custom.extend(eager.iter().cloned());
            }
            self.tools.extend(eager);
            self.enable_deferred_mcp_tools(deferred);
            self.refresh_subagent_tool();
        } else {
            if let Some(ref mut custom) = self.subagent_child_tools {
                custom.extend(mcp_tools.iter().cloned());
            }
            self.tools.extend(mcp_tools);
            if enable_codemode {
                let codemode_tool = self.create_codemode_tool(self.tools.clone());
                self.codemode_tool = Some(codemode_tool.clone());
                self.tools.push(codemode_tool);
            } else {
                self.codemode_tool = None;
            }
            self.rebuild_system_prompt();
            self.refresh_subagent_tool();
        }
    }

    pub(super) fn create_codemode_tool(
        &self,
        tools: Vec<Arc<dyn AgentTool>>,
    ) -> Arc<mehh_codemode::CodemodeTool> {
        let inspection_mgr = self.inspection_manager.clone();
        let approval_slot = self.approval_handler_slot.clone();
        let session_cwd = self.cwd.clone();
        let active_cancel_slot = self.active_cancel.clone();

        let mut codemode = mehh_codemode::CodemodeTool::new(tools);
        // Read the live tool list on every call: `replace_tools` must be able to
        // refresh `is_read_only` without rebuilding the interceptor.
        let live_tools = codemode.tools_handle();
        let interceptor: mehh_codemode::ToolCallInterceptor = Arc::new(move |name, args| {
            let inspection_mgr = inspection_mgr.clone();
            let approval_slot = approval_slot.clone();
            let session_cwd = session_cwd.clone();
            let active_cancel_slot = active_cancel_slot.clone();
            let live_tools = live_tools.clone();
            let name_str = name.to_string();
            let args_map = args.clone();
            Box::pin(async move {
                let is_read_only = live_tools
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|t| t.name() == name_str)
                    .and_then(|t| t.is_safe_for_auto_approve(&args_map));
                // Approval may be installed after the tool was created (TUI
                // calls set_approval_handler on session startup).
                let approval_h = approval_slot
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let cancel = mehh_ai::lock_recover(&active_cancel_slot)
                    .clone()
                    .unwrap_or_default();
                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id: format!("codemode/{name_str}"),
                    tool_name: name_str,
                    args: args_map,
                    is_read_only,
                    cwd: session_cwd,
                    origin: Some("codemode".to_string()),
                };
                let res = inspection_mgr
                    .inspect_and_authorize_cancellable(&inspect_ctx, approval_h.as_deref(), &cancel)
                    .await;
                if res.block {
                    Err(res
                        .reason
                        .unwrap_or_else(|| "Tool call blocked by inspection policy".to_string()))
                } else {
                    // Hand the approved target back so the codemode executor can
                    // bind it into the nested call before the tool writes.
                    Ok(res.approved_path)
                }
            })
        });

        codemode.set_interceptor(Some(interceptor));
        Arc::new(codemode)
    }

    /// Synchronizes updated MCP tools into the session, updating deferred catalog and codemode bindings in-place.
    pub fn sync_mcp_tools(&mut self, mcp_tools: Vec<Arc<dyn AgentTool>>, enable_codemode: bool) {
        strip_mcp_tools(&mut self.tools);
        if let Some(ref mut custom) = self.subagent_child_tools {
            strip_mcp_tools(custom);
        }

        let mcp_server_tool_count = mcp_tools
            .iter()
            .filter(|t| t.name().starts_with("mcp__"))
            .count();

        if !enable_codemode && mcp_server_tool_count > DEFERRED_MCP_TOOL_THRESHOLD {
            self.codemode_tool = None;
            let mut eager = Vec::new();
            let mut deferred = Vec::new();
            for tool in mcp_tools {
                if tool.name().starts_with("mcp__") {
                    deferred.push(tool);
                } else {
                    eager.push(tool);
                }
            }
            if let Some(ref mut custom) = self.subagent_child_tools {
                custom.extend(eager.iter().cloned());
            }
            self.tools.extend(eager);
            self.enable_deferred_mcp_tools(deferred);
            self.refresh_subagent_tool();
        } else {
            if self.deferred_catalog.is_some() {
                self.clear_deferred_mcp_tools();
            }
            if let Some(ref mut custom) = self.subagent_child_tools {
                custom.extend(mcp_tools.iter().cloned());
            }
            self.tools.extend(mcp_tools);
            if enable_codemode {
                if let Some(ref codemode) = self.codemode_tool {
                    codemode.replace_tools(self.tools.clone());
                    self.tools.push(codemode.clone());
                } else {
                    let codemode_tool = self.create_codemode_tool(self.tools.clone());
                    self.codemode_tool = Some(codemode_tool.clone());
                    self.tools.push(codemode_tool);
                }
            } else {
                self.codemode_tool = None;
            }
            self.rebuild_system_prompt();
            self.refresh_subagent_tool();
        }
    }

    /// Returns the active CodemodeTool handle, if enabled.
    pub fn codemode_tool(&self) -> Option<Arc<mehh_codemode::CodemodeTool>> {
        self.codemode_tool.clone()
    }

    /// Returns the deferred tool catalog handle, if active.
    pub fn deferred_catalog(&self) -> Option<Arc<DeferredToolCatalog>> {
        self.deferred_catalog.clone()
    }

    /// Clears deferred `tool_search` tools prior to resynchronizing MCP tools.
    pub fn clear_deferred_mcp_tools(&mut self) {
        mehh_ai::write_recover(&self.dynamic_tools).clear();
        self.deferred_catalog = None;
        self.tools.retain(|t| t.name() != "tool_search");
        if let Some(ref mut custom) = self.subagent_child_tools {
            custom.retain(|t| t.name() != "tool_search");
        }
    }

    /// Registers deferred tools behind `tool_search` and links them into `dynamic_tools`.
    pub fn enable_deferred_mcp_tools(&mut self, deferred: Vec<Arc<dyn AgentTool>>) {
        if deferred.is_empty() {
            return;
        }
        if let Some(ref catalog) = self.deferred_catalog {
            catalog.replace_deferred(deferred);
            if !self.tools.iter().any(|t| t.name() == "tool_search") {
                self.tools
                    .push(Arc::new(ToolSearchTool::new(catalog.clone())));
            }
        } else {
            let catalog = Arc::new(DeferredToolCatalog::new(deferred));
            self.dynamic_tools = catalog.dynamic_tools_handle();
            self.deferred_catalog = Some(Arc::clone(&catalog));
            self.tools.retain(|t| t.name() != "tool_search");
            self.tools.push(Arc::new(ToolSearchTool::new(catalog)));
        }
        if let (Some(ref mut custom), Some(tool_search)) = (
            self.subagent_child_tools.as_mut(),
            self.tools.iter().find(|t| t.name() == "tool_search"),
        ) {
            custom.retain(|t| t.name() != "tool_search");
            custom.push(tool_search.clone());
        }
        self.rebuild_system_prompt();
    }

    fn build_subagent_tool(&self, child_tools: Vec<Arc<dyn AgentTool>>) -> Arc<dyn AgentTool> {
        let filtered: Vec<Arc<dyn AgentTool>> = child_tools
            .into_iter()
            .filter(|t| t.name() != "subagent" && t.name() != "codemode")
            .collect();
        let (subagent_model, subagent_provider, subagent_api_key) = {
            let lw = self.lead_worker.lock().unwrap_or_else(|e| e.into_inner());
            let wm = lw
                .worker_model()
                .cloned()
                .unwrap_or_else(|| self.model.clone());
            let wp = lw
                .worker_provider()
                .cloned()
                .unwrap_or_else(|| self.provider.clone());
            let ak = subagent_api_key(
                &lw,
                &self.api_key,
                wm.provider.as_str(),
                self.model.provider.as_str(),
            );
            (wm, wp, ak)
        };
        Arc::new(
            SubagentTool::new(
                self.cwd.clone(),
                subagent_model,
                subagent_provider,
                subagent_api_key,
                filtered,
                0,
            )
            .with_inspection_manager(self.inspection_manager.clone())
            .with_approval_handler(self.approval_handler.clone())
            .with_control(self.subagent_control.clone())
            .with_lead_worker(self.lead_worker.clone()),
        )
    }

    /// Attaches or replaces the `subagent` tool on the parent session using an
    /// explicit `child_tools` set, without altering the parent's other tools.
    pub fn set_subagent_with_child_tools(&mut self, child_tools: Vec<Arc<dyn AgentTool>>) {
        self.subagent_child_tool_allowlist = Some(
            child_tools.iter().map(|t| t.name().to_string()).collect(),
        );
        self.subagent_child_tools = Some(child_tools.clone());
        let updated = self.build_subagent_tool(child_tools);
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == "subagent") {
            *slot = updated;
        } else {
            self.tools.push(updated);
        }
        if let Some(ref codemode) = self.codemode_tool {
            codemode.replace_tools(self.tools.clone());
        }
        self.rebuild_system_prompt();
    }

    /// Returns the explicit child tool slice configured for subagents, if any.
    pub fn subagent_child_tools(&self) -> Option<&[Arc<dyn AgentTool>]> {
        self.subagent_child_tools.as_deref()
    }

    pub(super) fn refresh_subagent_tool(&mut self) {
        let child_tools = self
            .subagent_child_tools
            .clone()
            .unwrap_or_else(|| self.tools.clone());
        let updated = self.build_subagent_tool(child_tools);
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == "subagent") {
            *slot = updated;
        }
        if let Some(ref codemode) = self.codemode_tool {
            codemode.replace_tools(self.tools.clone());
        }
    }
}
