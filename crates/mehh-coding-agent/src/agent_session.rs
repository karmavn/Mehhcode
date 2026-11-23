//! Orchestrates an agent session with tool executions, model streaming, and history tracking.

/// Marker used by `TodoState::format_moim_block` and by the idempotent injector below.
const MOIM_START: &str = "<working_checklist>";
const MOIM_END: &str = "</working_checklist>";

/// Replaces a previously injected MOIM block in place, or appends it once.
fn replace_moim_text(text: &mut String, block: &str) {
    if let Some(start) = text.find(MOIM_START) {
        if let Some(rel_end) = text[start..].find(MOIM_END) {
            let end = start + rel_end + MOIM_END.len();
            text.replace_range(start..end, block);
            return;
        }
    }
    text.push_str("\n\n");
    text.push_str(block);
}

/// Removes any previously injected MOIM block. Returns whether text changed.
fn strip_moim_text(text: &mut String) -> bool {
    if let Some(start) = text.find(MOIM_START) {
        if let Some(rel_end) = text[start..].find(MOIM_END) {
            let end = start + rel_end + MOIM_END.len();
            text.replace_range(start..end, "");
            return true;
        }
    }
    false
}

use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use mehh_agent::{run_agent_loop, AgentEvent, AgentLoopConfig, AgentTool, DEFAULT_MAX_AGENT_TURNS};
use mehh_ai::{
    default_http_client, is_context_overflow, now_ms, AnthropicProvider,
    AzureOpenAiResponsesProvider, BedrockProvider, FauxProvider, GoogleProvider, LlmProvider,
    Message, MistralProvider, Model, OpenAiCompletionsProvider, OpenAiResponsesProvider,
    ThinkingLevel, UserContent, UserMessage,
};

use crate::auth::resolve_api_key_async;
use crate::prompt_templates::PromptTemplate;
use crate::session::SessionManager;
use crate::settings::Settings;
use crate::skills::Skill;
use crate::subagent::SubagentTool;
use crate::subagent_control::{SubagentControl, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
use crate::system_prompt::build_system_prompt_with_skills_and_context;
use crate::tool_search::{DeferredToolCatalog, ToolSearchTool};

/// MCP tool count above which `mcp__*` tools are registered behind `tool_search`.
pub const DEFERRED_MCP_TOOL_THRESHOLD: usize = 20;

/// High-level agent session that pairs conversation history, model configuration,
/// system prompt assembly, tools, skills, and prompt templates.
pub struct AgentSession {
    pub cwd: PathBuf,
    pub model: Model,
    pub provider: Arc<dyn LlmProvider>,
    pub session: SessionManager,
    pub settings: Settings,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub system_prompt: String,
    pub custom_system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub api_key: Option<String>,
    has_explicit_api_key: bool,
    pub thinking_level: Option<ThinkingLevel>,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub no_context_files: bool,
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
    pub follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    pub active_cancel: Arc<Mutex<Option<CancellationToken>>>,
    pub active_bash_cancel: Arc<Mutex<Option<CancellationToken>>>,
    pub scoped_models: Vec<Model>,
    pub trusted: bool,
    dynamic_tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
    deferred_catalog: Option<Arc<DeferredToolCatalog>>,
    codemode_tool: Option<Arc<mehh_codemode::CodemodeTool>>,
    pub inspection_manager: Arc<crate::tool_inspection::ToolInspectionManager>,
    pub approval_handler: Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>,
    /// Shared with the codemode interceptor so `set_approval_handler` takes
    /// effect without recreating the sandbox tool.
    approval_handler_slot: Arc<RwLock<Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>>>,
    pub todo_state: Arc<tokio::sync::RwLock<crate::todo::TodoState>>,
    pub lead_worker: Arc<Mutex<crate::lead_worker::LeadWorkerManager>>,
    pub subagent_control: Arc<SubagentControl>,
}

/// Resolves the credential that a cross-provider subagent worker must use.
/// This must never fall back to the Lead key: at turn zero `active_api_key()`
/// is the Lead key even when the worker model uses a different provider.
fn subagent_api_key(
    manager: &crate::lead_worker::LeadWorkerManager,
    api_key: &Option<String>,
    worker_provider: &str,
    session_provider: &str,
) -> Option<String> {
    if worker_provider.eq_ignore_ascii_case(session_provider) {
        api_key.clone()
    } else {
        manager.worker_api_key.clone()
    }
}

impl AgentSession {
    /// Creates a new agent session assuming project trust (`trusted = true` for existing callers/tests).
    /// CLI and untrusted entrypoints should call [`AgentSession::new_with_trust`].
    pub fn new(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        session: SessionManager,
        settings: Settings,
    ) -> Self {
        Self::new_with_trust(cwd, model, provider, session, settings, true)
    }

    /// Creates a new agent session loading repo-local skills and prompt templates only when `trusted` is true.
    pub fn new_with_trust(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        session: SessionManager,
        settings: Settings,
        trusted: bool,
    ) -> Self {
        let skills = crate::skills::load_skills_with_trust(&cwd, trusted);
        let prompt_templates =
            crate::prompt_templates::load_prompt_templates_with_trust(&cwd, trusted);
        let mut tools = mehh_tools::create_coding_tools(cwd.clone());

        let thinking_level = settings.default_thinking_level;
        let default_mode = if model.provider == "faux" || model.api == "faux" {
            crate::tool_inspection::PermissionMode::Auto
        } else {
            crate::tool_inspection::PermissionMode::SmartApprove
        };
        let permission_mode = settings.permission_mode.unwrap_or(default_mode);
        let perm_store = crate::permissions::PermissionStore::load_fail_closed(&cwd, trusted);
        let inspection_manager = Arc::new(
            crate::tool_inspection::ToolInspectionManager::new_with_store(
                permission_mode,
                cwd.clone(),
                perm_store,
            ),
        );

        let (worker_model, worker_provider) = if let Some(ref wm) = settings.worker_model {
            if let Ok(reg) = crate::models_config::ModelRegistry::load_default() {
                if let Some(m) = reg.resolve(wm, None) {
                    let p = Self::default_provider_for_model(&m);
                    (Some(m), Some(p))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let worker_api_key = if let Some(ref wm) = worker_model {
            if wm.provider == "faux" || wm.api == "faux" {
                None
            } else {
                crate::auth::resolve_api_key(&wm.provider, None, None)
            }
        } else {
            None
        };

        let mut lw_mgr = crate::lead_worker::LeadWorkerManager::new(
            model.clone(),
            provider.clone(),
            worker_model,
            worker_provider,
            settings.lead_turns.unwrap_or(1),
        );
        lw_mgr.set_worker_api_key(worker_api_key.clone());
        let lead_worker = Arc::new(Mutex::new(lw_mgr));

        let subagent_model = lead_worker
            .lock()
            .unwrap()
            .worker_model()
            .cloned()
            .unwrap_or_else(|| model.clone());
        let subagent_provider = lead_worker
            .lock()
            .unwrap()
            .worker_provider()
            .cloned()
            .unwrap_or_else(|| provider.clone());
        let subagent_key = if subagent_model.provider == model.provider {
            None
        } else {
            worker_api_key
        };

        let subagent_control = Arc::new(SubagentControl::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS));
        let subagent = Arc::new(
            SubagentTool::new(
                cwd.clone(),
                subagent_model,
                subagent_provider,
                subagent_key,
                tools.clone(),
                0,
            )
            .with_inspection_manager(inspection_manager.clone())
            .with_control(subagent_control.clone())
            .with_lead_worker(lead_worker.clone()),
        );
        tools.push(subagent);
        let todo_state = Arc::new(tokio::sync::RwLock::new(crate::todo::TodoState::default()));
        let todo_tool = Arc::new(crate::todo::TodoTool::new(todo_state.clone()));
        tools.push(todo_tool);
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let system_prompt = build_system_prompt_with_skills_and_context(
            &cwd, None, None, &tool_names, &skills, trusted,
        );

        Self {
            cwd,
            model,
            provider,
            session,
            settings,
            tools,
            system_prompt,
            custom_system_prompt: None,
            append_system_prompt: None,
            api_key: None,
            has_explicit_api_key: false,
            thinking_level,
            skills,
            prompt_templates,
            no_context_files: false,
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            active_cancel: Arc::new(Mutex::new(None)),
            active_bash_cancel: Arc::new(Mutex::new(None)),
            scoped_models: Vec::new(),
            trusted,
            dynamic_tools: Arc::new(RwLock::new(Vec::new())),
            deferred_catalog: None,
            codemode_tool: None,
            inspection_manager,
            approval_handler: None,
            approval_handler_slot: Arc::new(RwLock::new(None)),
            todo_state,
            lead_worker,
            subagent_control,
        }
    }

    /// Queues a steering user message to be processed at the next turn or tool call boundary.
    pub fn steer(&self, text: impl Into<String>) {
        let msg = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        self.steering_queue.lock().unwrap().push_back(msg);
    }

    /// Queues a follow-up user message to be processed once the current turn completes.
    pub fn follow_up(&self, text: impl Into<String>) {
        let msg = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        self.follow_up_queue.lock().unwrap().push_back(msg);
    }

    /// Returns the number of pending steering messages.
    pub fn pending_steering_count(&self) -> usize {
        self.steering_queue.lock().unwrap().len()
    }

    /// Returns the number of pending follow-up messages.
    pub fn pending_follow_up_count(&self) -> usize {
        self.follow_up_queue.lock().unwrap().len()
    }

    /// Clears both steering and follow-up queues.
    pub fn clear_queues(&self) {
        self.steering_queue.lock().unwrap().clear();
        self.follow_up_queue.lock().unwrap().clear();
    }

    /// Aborts any currently active running prompt loop. Returns true if a cancel was triggered.
    ///
    /// The cancelled token stays in the slot. That lets the next turn's
    /// `prepare_prompt_content` inherit the cancellation when Ctrl+C lands in
    /// the handoff window after the UI already checked the preparation token.
    pub fn abort(&self) -> bool {
        let guard = self.active_cancel.lock().unwrap();
        match guard.as_ref() {
            Some(c) => {
                let was_active = !c.is_cancelled();
                c.cancel();
                was_active
            }
            None => false,
        }
    }

    /// Returns whether a prompt execution is currently in progress and not cancelled.
    pub fn is_active(&self) -> bool {
        self.active_cancel
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| !c.is_cancelled())
            .unwrap_or(false)
    }

    /// Sets the list of scoped models available for cycling.
    pub fn set_scoped_models(&mut self, models: Vec<Model>) {
        self.scoped_models = models;
    }

    /// Returns a reference to the scoped models list.
    pub fn scoped_models(&self) -> &[Model] {
        &self.scoped_models
    }

    /// Cycles to the next model in `scoped_models` (or registry if scoped_models is empty).
    pub fn cycle_model(&mut self) -> Result<Option<Model>> {
        let models_list: Vec<Model> = if !self.scoped_models.is_empty() {
            self.scoped_models.clone()
        } else if let Ok(reg) = crate::models_config::ModelRegistry::load_default() {
            reg.models().to_vec()
        } else {
            Vec::new()
        };

        if models_list.is_empty() {
            return Ok(None);
        }

        let curr_idx = models_list.iter().position(|m| {
            m.id.eq_ignore_ascii_case(&self.model.id)
                && m.provider.eq_ignore_ascii_case(&self.model.provider)
        });
        let next_idx = match curr_idx {
            Some(i) => (i + 1) % models_list.len(),
            None => 0,
        };
        let next_model = models_list[next_idx].clone();
        let prov = Self::default_provider_for_model(&next_model);
        self.set_model(next_model.clone(), prov);
        Ok(Some(next_model))
    }

    /// Aborts any currently running bash tool command.
    pub fn abort_bash(&self) {
        if let Some(tok) = self.active_bash_cancel.lock().unwrap().take() {
            tok.cancel();
        }
    }

    /// Returns the shared steering queue.
    pub fn steering_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.steering_queue.clone()
    }

    /// Returns the shared follow-up queue.
    pub fn follow_up_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.follow_up_queue.clone()
    }

    /// Returns the shared subagent control (registry + global budget).
    pub fn subagent_control(&self) -> Arc<SubagentControl> {
        self.subagent_control.clone()
    }

    /// Returns the slot holding the active cancellation token.
    pub fn active_cancel_slot(&self) -> Arc<Mutex<Option<CancellationToken>>> {
        self.active_cancel.clone()
    }

    /// Sets or clears the interactive approval handler for tool inspection.
    pub fn set_approval_handler(
        &mut self,
        handler: Option<Arc<dyn crate::tool_inspection::ApprovalHandler>>,
    ) {
        self.approval_handler = handler;
        *self.approval_handler_slot.write().unwrap() = self.approval_handler.clone();
        self.refresh_subagent_tool();
    }

    /// Overrides active tools for the session.
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) {
        self.tools = tools;
        self.refresh_subagent_tool();
        if let Some(ref codemode) = self.codemode_tool {
            codemode.replace_tools(self.tools.clone());
        }
        self.rebuild_system_prompt();
    }

    /// Returns a slice of the active tools.
    pub fn tools(&self) -> &[Arc<dyn AgentTool>] {
        &self.tools
    }

    /// Stores custom/append system prompt overrides and rebuilds the active system prompt.
    pub fn set_custom_system_prompt(&mut self, custom: Option<String>, append: Option<String>) {
        self.custom_system_prompt = custom;
        self.append_system_prompt = append;
        self.rebuild_system_prompt();
    }

    /// Rebuilds the structured system prompt from current tools, skills, and preserved prompt overrides.
    pub fn rebuild_system_prompt(&mut self) {
        let tool_names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
        self.system_prompt = crate::system_prompt::build_system_prompt_with_skills_and_context(
            &self.cwd,
            self.custom_system_prompt.as_deref(),
            self.append_system_prompt.as_deref(),
            &tool_names,
            &self.skills,
            self.trusted && !self.no_context_files,
        );
    }

    /// Skips loading AGENTS.md / CLAUDE.md into the system prompt.
    pub fn set_no_context_files(&mut self, value: bool) {
        self.no_context_files = value;
        self.rebuild_system_prompt();
    }

    /// Reloads settings, skills, and prompt templates from disk, then rebuilds system prompt.
    pub fn reload(&mut self) {
        if let Ok(settings) =
            crate::settings::SettingsManager::try_load_with_trust(&self.cwd, self.trusted)
        {
            self.settings = settings;
        }
        self.skills = crate::skills::load_skills_with_trust(&self.cwd, self.trusted);
        self.prompt_templates =
            crate::prompt_templates::load_prompt_templates_with_trust(&self.cwd, self.trusted);
        self.rebuild_system_prompt();
    }

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
            self.tools.extend(eager);
            self.enable_deferred_mcp_tools(deferred);
            self.refresh_subagent_tool();
        } else {
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

    fn create_codemode_tool(
        &self,
        tools: Vec<Arc<dyn AgentTool>>,
    ) -> Arc<mehh_codemode::CodemodeTool> {
        let inspection_mgr = self.inspection_manager.clone();
        let approval_slot = self.approval_handler_slot.clone();
        let session_cwd = self.cwd.clone();

        let mut codemode = mehh_codemode::CodemodeTool::new(tools);
        // Read the live tool list on every call: `replace_tools` must be able to
        // refresh `is_read_only` without rebuilding the interceptor.
        let live_tools = codemode.tools_handle();
        let interceptor: mehh_codemode::ToolCallInterceptor = Arc::new(move |name, args| {
            let inspection_mgr = inspection_mgr.clone();
            let approval_slot = approval_slot.clone();
            let session_cwd = session_cwd.clone();
            let live_tools = live_tools.clone();
            let name_str = name.to_string();
            let args_map = args.clone();
            Box::pin(async move {
                let is_read_only = live_tools
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|t| t.name() == name_str)
                    .and_then(|t| t.is_read_only());
                // Approval may be installed after the tool was created (TUI
                // calls set_approval_handler on session startup).
                let approval_h = approval_slot
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id: format!("codemode/{name_str}"),
                    tool_name: name_str,
                    args: args_map,
                    is_read_only,
                    cwd: session_cwd,
                    origin: Some("codemode".to_string()),
                };
                let res = inspection_mgr
                    .inspect_and_authorize(&inspect_ctx, approval_h.as_deref())
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
        self.tools.retain(|t| {
            !t.name().starts_with("mcp__")
                && t.name() != "list_mcp_resources"
                && t.name() != "list_mcp_resource_templates"
                && t.name() != "read_mcp_resource"
                && t.name() != "tool_search"
                && t.name() != "codemode"
        });

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
            self.tools.extend(eager);
            self.enable_deferred_mcp_tools(deferred);
            self.refresh_subagent_tool();
        } else {
            if self.deferred_catalog.is_some() {
                self.clear_deferred_mcp_tools();
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
        self.dynamic_tools.write().unwrap().clear();
        self.deferred_catalog = None;
        self.tools.retain(|t| t.name() != "tool_search");
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
        self.rebuild_system_prompt();
    }

    fn refresh_subagent_tool(&mut self) {
        let base_tools: Vec<Arc<dyn AgentTool>> = self
            .tools
            .iter()
            .filter(|t| t.name() != "subagent" && t.name() != "codemode")
            .cloned()
            .collect();
        let (subagent_model, subagent_provider, subagent_api_key) = {
            let lw = self.lead_worker.lock().unwrap();
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
        let updated = Arc::new(
            SubagentTool::new(
                self.cwd.clone(),
                subagent_model,
                subagent_provider,
                subagent_api_key,
                base_tools,
                0,
            )
            .with_inspection_manager(self.inspection_manager.clone())
            .with_approval_handler(self.approval_handler.clone())
            .with_control(self.subagent_control.clone())
            .with_lead_worker(self.lead_worker.clone()),
        );
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == "subagent") {
            *slot = updated;
        }
        if let Some(ref codemode) = self.codemode_tool {
            codemode.replace_tools(self.tools.clone());
        }
    }

    /// Sets or updates the system prompt directly.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
    }

    /// Sets or updates the provider API key (clearing any explicit CLI override marker).
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = false;
        self.api_key = api_key.clone();
        self.lead_worker.lock().unwrap().set_lead_api_key(api_key);
        self.refresh_subagent_tool();
    }

    /// Sets an explicit CLI `--api-key` override that must not be clobbered on same-provider switches.
    pub fn set_explicit_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = api_key.as_deref().is_some_and(|k| !k.trim().is_empty());
        self.api_key = api_key.clone();
        self.lead_worker.lock().unwrap().set_lead_api_key(api_key);
        self.refresh_subagent_tool();
    }

    /// Returns whether the active key was explicitly pinned by CLI `--api-key`.
    pub fn has_explicit_api_key(&self) -> bool {
        self.has_explicit_api_key
    }

    /// Sets or updates reasoning/thinking level.
    pub fn set_thinking_level(&mut self, level: Option<ThinkingLevel>) {
        self.thinking_level = level;
    }

    /// Updates both model metadata and its backing provider implementation.
    /// Clears `api_key` whenever switching to a different provider so keys never leak across providers.
    pub fn set_model(&mut self, model: Model, provider: Arc<dyn LlmProvider>) {
        if !self.model.provider.eq_ignore_ascii_case(&model.provider) {
            self.api_key = None;
            self.has_explicit_api_key = false;
            self.lead_worker.lock().unwrap().set_lead_api_key(None);
        }
        self.model = model;
        self.provider = provider;
        {
            let mut mgr = self.lead_worker.lock().unwrap();
            mgr.lead_model = self.model.clone();
            mgr.lead_provider = self.provider.clone();
        }
        self.refresh_subagent_tool();
    }

    /// Returns a reference to the active session manager.
    pub fn session(&self) -> &SessionManager {
        &self.session
    }

    /// Returns a mutable reference to the active session manager.
    pub fn session_mut(&mut self) -> &mut SessionManager {
        &mut self.session
    }

    /// Returns the currently active model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Returns the active thinking/reasoning level.
    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.thinking_level
    }

    /// Returns the slice of loaded skills.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Returns the slice of loaded prompt templates.
    pub fn prompt_templates(&self) -> &[PromptTemplate] {
        &self.prompt_templates
    }

    /// Resolves the default provider implementation for the given model metadata.
    pub fn default_provider_for_model(model: &Model) -> Arc<dyn LlmProvider> {
        if model.provider == "faux" || model.api == "faux" {
            Arc::new(FauxProvider::default())
        } else if model.api == "mistral-conversations" || model.provider == "mistral" {
            Arc::new(MistralProvider::new())
        } else if model.api == "azure-openai-responses" || model.provider == "azure-openai" {
            Arc::new(AzureOpenAiResponsesProvider::new())
        } else if model.api == "bedrock-converse-stream"
            || model.provider == "amazon-bedrock"
            || model.provider == "bedrock"
        {
            Arc::new(BedrockProvider::new())
        } else if model.api == "google-vertex"
            || model.api == "google-generative-ai"
            || model.provider == "google"
            || model.provider == "gemini"
        {
            Arc::new(GoogleProvider::new())
        } else if model.api == "anthropic-messages" || model.provider == "anthropic" {
            Arc::new(AnthropicProvider::new(default_http_client()))
        } else if model.api == "openai-responses" {
            Arc::new(OpenAiResponsesProvider::new())
        } else {
            Arc::new(OpenAiCompletionsProvider::new(default_http_client()))
        }
    }

    /// Expands `/skill:` commands and prompt template commands in the user input.
    pub fn expand_input(&self, input: &str) -> String {
        if let Some(expanded) = crate::skills::expand_skill_command(input, &self.skills) {
            return expanded;
        }
        if let Some(expanded) =
            crate::prompt_templates::expand_prompt_template(input, &self.prompt_templates)
        {
            return expanded;
        }
        input.to_string()
    }

    /// Prepares input, history, and agent loop configuration for a prompt turn with arbitrary user content.
    pub fn prepare_prompt_content(
        &self,
        content: UserContent,
    ) -> (Message, Vec<Message>, AgentLoopConfig) {
        // Routing decisions for the first turn must see a fresh prompt counter.
        self.lead_worker.lock().unwrap().reset_for_new_prompt();
        let expanded_content = match content {
            UserContent::Text(text) => UserContent::Text(self.expand_input(&text)),
            UserContent::Blocks(blocks) => {
                let expanded_blocks = blocks
                    .into_iter()
                    .map(|b| match b {
                        mehh_ai::UserContentBlock::Text(t) => {
                            mehh_ai::UserContentBlock::Text(mehh_ai::TextContent {
                                text: self.expand_input(&t.text),
                                text_signature: t.text_signature,
                            })
                        }
                        image => image,
                    })
                    .collect();
                UserContent::Blocks(expanded_blocks)
            }
        };

        let user_msg = Message::User(UserMessage {
            content: expanded_content,
            timestamp: now_ms(),
        });

        let history = self.session.build_context_messages();

        let cancel = CancellationToken::new();
        {
            let mut slot = self.active_cancel.lock().unwrap();
            // Idempotent handoff: a cancelled preparation token means Ctrl+C
            // fired before this loop token existed. Carry the cancellation
            // over instead of blindly replacing it and running the turn.
            if slot.as_ref().is_some_and(|prev| prev.is_cancelled()) {
                cancel.cancel();
            }
            *slot = Some(cancel.clone());
        }

        let mut config = AgentLoopConfig::new(self.model.clone(), self.provider.clone());
        config.steering_queue = self.steering_queue.clone();
        config.follow_up_queue = self.follow_up_queue.clone();
        config.steering_mode = self.settings.steering_mode.unwrap_or_default();
        config.follow_up_mode = self.settings.follow_up_mode.unwrap_or_default();
        config.cancel = cancel;
        config.system_prompt = Some(self.system_prompt.clone());
        config.tools = self.tools.clone();
        config.dynamic_tools = Some(Arc::clone(&self.dynamic_tools));
        config.max_turns = Some(DEFAULT_MAX_AGENT_TURNS);
        config.stream_options.api_key = self.api_key.clone();
        {
            let mgr = self.lead_worker.lock().unwrap();
            if mgr.should_use_worker() {
                config.model = mgr.active_model();
                config.provider = mgr.active_provider();
                config.stream_options.api_key = mgr.worker_api_key.clone();
            }
        }
        config.stream_options.reasoning = self.thinking_level;
        if let Some(t) = self.settings.temperature {
            config.stream_options.temperature = Some(t);
        }
        if let Some(m) = self.settings.max_tokens {
            config.stream_options.max_tokens = Some(m);
        }

        let inspection_mgr = self.inspection_manager.clone();
        let approval_h = self.approval_handler.clone();
        let session_cwd = self.cwd.clone();

        config.before_tool_call = Some(Arc::new(move |before_ctx, _cancel| {
            let inspection_mgr = inspection_mgr.clone();
            let approval_h = approval_h.clone();
            let session_cwd = session_cwd.clone();

            Box::pin(async move {
                let tool_name = before_ctx.tool_call.name.clone();
                let tool_call_id = before_ctx.tool_call.id.clone();
                let is_read_only = before_ctx
                    .context
                    .tools
                    .as_ref()
                    .and_then(|tools| tools.iter().find(|t| t.name() == tool_name))
                    .and_then(|t| t.is_read_only());

                let inspect_ctx = crate::tool_inspection::ToolInspectionContext {
                    tool_call_id,
                    tool_name,
                    args: before_ctx.args,
                    is_read_only,
                    cwd: session_cwd,
                    origin: None,
                };

                inspection_mgr
                    .inspect_and_authorize(&inspect_ctx, approval_h.as_deref())
                    .await
            })
        }));

        let lead_worker_outcomes = self.lead_worker.clone();
        config.after_tool_call = Some(Arc::new(move |after_ctx, _cancel| {
            lead_worker_outcomes
                .lock()
                .unwrap()
                .record_tool_outcome(after_ctx.is_error);
            Box::pin(async { mehh_agent::types::AfterToolCallResult::default() })
        }));

        let todo_state = self.todo_state.clone();
        let session_cwd_moim = self.cwd.clone();
        let session_trusted = self.trusted;

        config.prepare_request = Some(Arc::new(move |req_ctx, _cancel| {
            let todo_state = todo_state.clone();
            let session_cwd = session_cwd_moim.clone();

            Box::pin(async move {
                let block_opt = {
                    let read = todo_state.read().await;
                    read.format_moim_block(&session_cwd, session_trusted)
                };

                let mut ctx = req_ctx.context.clone();
                let mut changed = false;
                if let Some(ref moim_block) = block_opt {
                    if let Some(Message::User(ref mut u)) = ctx
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|m| matches!(m, Message::User(_)))
                    {
                        match u.content {
                            UserContent::Text(ref mut t) => {
                                replace_moim_text(t, moim_block);
                                changed = true;
                            }
                            UserContent::Blocks(ref mut blocks) => {
                                if let Some(t) = blocks.iter_mut().find_map(|b| match b {
                                    mehh_ai::UserContentBlock::Text(t)
                                        if t.text.contains(MOIM_START) =>
                                    {
                                        Some(t)
                                    }
                                    _ => None,
                                }) {
                                    replace_moim_text(&mut t.text, moim_block);
                                } else {
                                    blocks.push(mehh_ai::UserContentBlock::Text(
                                        mehh_ai::TextContent {
                                            text: format!("\n\n{}", moim_block),
                                            text_signature: None,
                                        },
                                    ));
                                }
                                changed = true;
                            }
                        }
                    } else {
                        // Fallback: If no User message exists in history, insert at the beginning.
                        ctx.messages.insert(
                            0,
                            Message::User(UserMessage {
                                content: UserContent::Text(moim_block.clone()),
                                timestamp: now_ms(),
                            }),
                        );
                        changed = true;
                    }
                } else {
                    // Checklist cleared: remove the stale injected block as well.
                    for msg in ctx.messages.iter_mut() {
                        if let Message::User(u) = msg {
                            match u.content {
                                UserContent::Text(ref mut t) => {
                                    changed |= strip_moim_text(t);
                                }
                                UserContent::Blocks(ref mut blocks) => {
                                    let before = blocks.len();
                                    blocks.retain(|b| match b {
                                        mehh_ai::UserContentBlock::Text(t) => {
                                            !t.text.contains(MOIM_START)
                                        }
                                        _ => true,
                                    });
                                    changed |= blocks.len() != before;
                                }
                            }
                        }
                    }
                }

                if changed {
                    Some(mehh_agent::AgentRequestUpdate {
                        context: Some(ctx),
                        ..Default::default()
                    })
                } else {
                    None
                }
            })
        }));

        let lead_worker_turn = self.lead_worker.clone();
        config.prepare_next_turn = Some(Arc::new(move |turn_ctx, _cancel| {
            let lead_worker = lead_worker_turn.clone();
            Box::pin(async move {
                let mut mgr = lead_worker.lock().unwrap();
                let stop_reason = turn_ctx.assistant_message.stop_reason;
                let is_empty_assistant = turn_ctx.assistant_message.content.is_empty();
                let repetition_detected = turn_ctx.tool_results.iter().any(|tr| {
                    tr.is_error
                        && tr.content.iter().any(|b| match b {
                            mehh_ai::ToolResultContentBlock::Text(t) => {
                                t.text.contains("Repeated identical tool call")
                            }
                            _ => false,
                        })
                });
                let is_repetition_break = is_empty_assistant || repetition_detected;
                mgr.record_turn_outcome(stop_reason, is_repetition_break);

                if mgr.is_active() {
                    let next_model = mgr.active_model();
                    let next_provider = mgr.active_provider();

                    if next_model.id != turn_ctx.assistant_message.model
                        || next_model.provider != turn_ctx.assistant_message.provider
                    {
                        let next_api_key = mgr.active_api_key();
                        Some(mehh_agent::AgentLoopTurnUpdate {
                            model: Some(next_model),
                            provider: Some(next_provider),
                            api_key: Some(next_api_key),
                            ..Default::default()
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        }));

        (user_msg, history, config)
    }

    /// Prepares input, history, and agent loop configuration for a prompt turn.
    pub fn prepare_prompt(&self, text: &str) -> (Message, Vec<Message>, AgentLoopConfig) {
        self.prepare_prompt_content(UserContent::Text(text.to_string()))
    }

    /// Appends new messages from an agent turn to the session manager.
    pub fn finish_prompt(&mut self, new_messages: &[Message]) -> Result<()> {
        for msg in new_messages {
            self.session.append_message(msg.clone())?;
        }
        Ok(())
    }

    /// Resolves and refreshes credentials asynchronously for `provider` unless overridden by `--api-key`.
    pub(crate) async fn refresh_resolved_api_key(
        provider: &str,
        model_api: &str,
        has_explicit_api_key: bool,
        auth_file: Option<&std::path::Path>,
    ) -> Result<Option<String>> {
        if has_explicit_api_key || provider == "faux" || model_api == "faux" {
            return Ok(None);
        }
        if let Some(key) = resolve_api_key_async(provider, None, auth_file).await? {
            return Ok(Some(key));
        }
        match crate::models_config::ModelRegistry::load_default() {
            Ok(reg) => Ok(reg.provider_api_key(provider).map(str::to_string)),
            Err(err)
                if std::env::var_os("USERPROFILE").is_none()
                    && std::env::var_os("HOME").is_none() =>
            {
                let _ = err;
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Resolves and refreshes `api_key` asynchronously (refreshing expiring OAuth tokens across turns).
    pub async fn ensure_api_key(&mut self, auth_file: Option<&std::path::Path>) -> Result<()> {
        if let Some(key) = Self::refresh_resolved_api_key(
            &self.model.provider,
            &self.model.api,
            self.has_explicit_api_key,
            auth_file,
        )
        .await?
        {
            if self.api_key.as_deref() != Some(key.as_str()) {
                self.api_key = Some(key);
                self.lead_worker
                    .lock()
                    .unwrap()
                    .set_lead_api_key(self.api_key.clone());
                self.refresh_subagent_tool();
            }
        }
        Ok(())
    }

    /// Prepares proactive auto-compaction metadata synchronously when threshold is reached.
    pub fn prepare_proactive_compaction(&self) -> Option<crate::compaction::CompactionPreparation> {
        let auto_compact_enabled = self.settings.auto_compact.unwrap_or(true);
        if !auto_compact_enabled {
            return None;
        }
        let comp_settings = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            auto_compact_enabled,
        );
        let current_tokens =
            crate::compaction::estimate_context_tokens(&self.session.build_context_messages())
                + self.system_prompt.chars().count().div_ceil(4)
                + crate::compaction::estimate_tool_definitions_tokens(&self.tools);
        if !crate::compaction::should_auto_compact(
            current_tokens,
            usize::try_from(self.model.context_window).unwrap_or(usize::MAX),
            &comp_settings,
        ) {
            return None;
        }
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();
        crate::compaction::prepare_compaction(&active_entries, comp_settings.keep_recent_tokens)
    }

    /// Prepares emergency overflow compaction metadata synchronously (falling back to 1024 recent tokens).
    pub fn prepare_overflow_compaction(&self) -> Option<crate::compaction::CompactionPreparation> {
        if !self.settings.auto_compact.unwrap_or(true) {
            return None;
        }
        let comp_settings = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            true,
        );
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();
        crate::compaction::prepare_compaction(&active_entries, comp_settings.keep_recent_tokens)
            .or_else(|| crate::compaction::prepare_compaction(&active_entries, 1024))
    }

    /// Persists a completed compaction entry into the active session branch.
    pub fn apply_prepared_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
    ) -> Result<()> {
        self.session
            .append_entry(crate::session::SessionEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            })?;
        Ok(())
    }

    /// Runs proactive auto-compaction before a turn when enabled (default `true`).
    pub async fn run_proactive_compaction(&mut self) {
        if let Some(prep) = self.prepare_proactive_compaction() {
            if let Err(e) = self.run_prepared_compaction(prep, None).await {
                eprintln!("Warning: proactive auto-compaction failed: {e}");
            }
        }
    }

    /// Returns true if `messages` ended due to context overflow and auto-compaction is enabled.
    pub fn is_overflow_turn(&self, messages: &[Message]) -> bool {
        let auto_compact_enabled = self.settings.auto_compact.unwrap_or(true);
        if !auto_compact_enabled {
            return false;
        }
        messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(ast) => Some(ast),
                _ => None,
            })
            .is_some_and(|ast| is_context_overflow(ast, self.model.context_window))
    }

    /// Attempts emergency compaction after a context overflow turn. Propagates provider/cancel errors.
    pub async fn try_overflow_compaction(&mut self) -> Result<bool> {
        let Some(prep) = self.prepare_overflow_compaction() else {
            return Ok(false);
        };
        self.run_prepared_compaction(prep, None).await?;
        Ok(true)
    }

    /// Sends user content through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt_content(
        &mut self,
        content: UserContent,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        let pre_cancel = CancellationToken::new();
        *self.active_cancel.lock().unwrap() = Some(pre_cancel.clone());

        let auth_res = tokio::select! {
            _ = pre_cancel.cancelled() => {
                *self.active_cancel.lock().unwrap() = None;
                anyhow::bail!("Operation aborted");
            }
            res = self.ensure_api_key(None) => res,
        };
        if let Err(e) = auth_res {
            *self.active_cancel.lock().unwrap() = None;
            return Err(e);
        }
        if pre_cancel.is_cancelled() {
            *self.active_cancel.lock().unwrap() = None;
            anyhow::bail!("Operation aborted");
        }

        self.run_proactive_compaction().await;
        if pre_cancel.is_cancelled() {
            *self.active_cancel.lock().unwrap() = None;
            anyhow::bail!("Operation aborted");
        }

        let (user_msg, history, config) = self.prepare_prompt_content(content.clone());
        let new_messages = run_agent_loop(vec![user_msg], history, config, on_event.clone()).await;
        *self.active_cancel.lock().unwrap() = None;

        if self.is_overflow_turn(&new_messages) {
            match self.try_overflow_compaction().await {
                Ok(true) => {
                    let (retry_user, retry_history, retry_config) =
                        self.prepare_prompt_content(content);
                    let retry_messages =
                        run_agent_loop(vec![retry_user], retry_history, retry_config, on_event)
                            .await;
                    *self.active_cancel.lock().unwrap() = None;
                    self.finish_prompt(&retry_messages)?;
                    return Ok(retry_messages);
                }
                Ok(false) => {}
                Err(e) => {
                    // The overflow turn must not be persisted: it is only a
                    // trigger for compaction, and retry deliberately avoids
                    // saving it. Surface the failure instead of swallowing it.
                    return Err(e).context("Overflow auto-compaction failed");
                }
            }
        }

        self.finish_prompt(&new_messages)?;
        Ok(new_messages)
    }

    /// Sends a user prompt through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt(
        &mut self,
        text: &str,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        self.prompt_content(UserContent::Text(text.to_string()), on_event)
            .await
    }

    /// Manually triggers compaction on the active branch history, saving the summary entry.
    pub async fn compact(&mut self, custom_instructions: Option<&str>) -> Result<Option<String>> {
        let keep = crate::compaction::CompactionSettings::for_context_window(
            self.model.context_window,
            true,
        )
        .keep_recent_tokens;
        self.compact_with_budget(custom_instructions, keep).await
    }

    async fn compact_with_budget(
        &mut self,
        custom_instructions: Option<&str>,
        keep_recent_tokens: usize,
    ) -> Result<Option<String>> {
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();

        let prep = match crate::compaction::prepare_compaction(&active_entries, keep_recent_tokens)
        {
            Some(p) => p,
            None => return Ok(None),
        };
        let summary = self
            .run_prepared_compaction(prep, custom_instructions)
            .await?;
        Ok(Some(summary))
    }

    async fn run_prepared_compaction(
        &mut self,
        prep: crate::compaction::CompactionPreparation,
        custom_instructions: Option<&str>,
    ) -> Result<String> {
        let existing = self.active_cancel.lock().unwrap().clone();
        let owned_slot = existing.is_none();
        let cancel = existing.unwrap_or_else(|| {
            let c = CancellationToken::new();
            *self.active_cancel.lock().unwrap() = Some(c.clone());
            c
        });
        let res = crate::compaction::compact_with_provider(
            &prep,
            &self.model,
            self.provider.as_ref(),
            self.api_key.as_deref(),
            custom_instructions,
            cancel,
        )
        .await;
        if owned_slot {
            *self.active_cancel.lock().unwrap() = None;
        }
        let summary = res?;
        self.apply_prepared_compaction(
            summary.clone(),
            prep.first_kept_entry_id,
            prep.tokens_before,
        )?;
        Ok(summary)
    }

    /// Navigates to a target entry in the session tree, optionally summarizing the abandoned branch.
    pub async fn navigate_tree(
        &mut self,
        target_id: &str,
        summarize: bool,
        custom_instructions: Option<&str>,
    ) -> Result<Option<String>> {
        let old_leaf_id = self.session.leaf_id().map(str::to_string);

        if self.session.get_entry(target_id).is_none() {
            return Err(anyhow!("Target entry ID not found: {target_id}"));
        }

        let summary = if summarize {
            let collected = crate::branch_summarization::collect_entries_for_branch_summary(
                &self.session,
                old_leaf_id.as_deref(),
                target_id,
            );
            if !collected.entries.is_empty() {
                let cancel = CancellationToken::new();
                let sum = crate::branch_summarization::generate_branch_summary(
                    &collected.entries,
                    &self.model,
                    self.provider.as_ref(),
                    self.api_key.as_deref(),
                    custom_instructions,
                    None,
                    cancel,
                )
                .await?;

                // Switch branch to target_id first
                self.session.branch_from(target_id)?;
                // Append BranchSummary on top of target branch
                let from_id = old_leaf_id.unwrap_or_else(|| "root".to_string());
                self.session
                    .append_entry(crate::session::SessionEntryKind::BranchSummary {
                        from_id,
                        summary: sum.clone(),
                    })?;
                Some(sum)
            } else {
                self.session.branch_from(target_id)?;
                None
            }
        } else {
            self.session.branch_from(target_id)?;
            None
        };

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEntryKind;
    use mehh_ai::{faux_model, StopReason};

    #[test]
    fn test_queues_and_abort() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_q_{}", uuid::Uuid::now_v7()));
        let session_mgr = SessionManager::in_memory(&temp_dir);
        let session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            session_mgr,
            Settings::default(),
        );

        assert_eq!(session.pending_steering_count(), 0);
        assert_eq!(session.pending_follow_up_count(), 0);
        assert!(!session.is_active());

        session.steer("Steer prompt");
        session.follow_up("Follow-up prompt");
        assert_eq!(session.pending_steering_count(), 1);
        assert_eq!(session.pending_follow_up_count(), 1);

        session.clear_queues();
        assert_eq!(session.pending_steering_count(), 0);
        assert_eq!(session.pending_follow_up_count(), 0);

        // Test cancel slot
        let cancel = CancellationToken::new();
        *session.active_cancel_slot().lock().unwrap() = Some(cancel.clone());
        assert!(session.is_active());
        assert!(session.abort());
        assert!(!session.is_active());
        assert!(cancel.is_cancelled());
        assert!(!session.abort());

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_prepare_prompt_content_inherits_aborted_prep_cancellation() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_handoff_{}", uuid::Uuid::now_v7()));
        let session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );

        // `start_turn_preparation` installs a preparation token in the slot.
        let prep_cancel = CancellationToken::new();
        *session.active_cancel_slot().lock().unwrap() = Some(prep_cancel.clone());
        assert!(session.is_active());

        // Ctrl+C lands after ReadyForTurn's `is_cancelled` check but before the
        // loop token is installed. abort() cancels the preparation token.
        assert!(session.abort());
        assert!(prep_cancel.is_cancelled());

        // `start_turn` -> `prepare_prompt_content` installs the loop token. It
        // must inherit the preparation cancellation instead of clearing it.
        let (_msg, _history, config) =
            session.prepare_prompt_content(UserContent::Text("retry".to_string()));
        assert!(
            config.cancel.is_cancelled(),
            "loop token must inherit cancellation from the aborted preparation token"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_context_files_injection_gated_by_trust() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_ctx_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let marker = "UNTRUSTED_AGENTS_MD_MARKER_7F3A";
        std::fs::write(
            temp_dir.join("AGENTS.md"),
            format!("Project rule {marker} must be followed."),
        )
        .unwrap();

        let mut untrusted = AgentSession::new_with_trust(
            temp_dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
            false,
        );
        assert!(
            !untrusted.system_prompt.contains(marker),
            "untrusted AGENTS.md must not be injected into the system prompt"
        );
        untrusted.rebuild_system_prompt();
        assert!(
            !untrusted.system_prompt.contains(marker),
            "rebuild must also keep untrusted AGENTS.md out of the system prompt"
        );

        let trusted = AgentSession::new_with_trust(
            temp_dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
            true,
        );
        assert!(
            trusted.system_prompt.contains(marker),
            "trusted AGENTS.md must be injected into the system prompt"
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_default_provider_routing() {
        let mut model = faux_model();
        model.api = "mistral-conversations".to_string();
        model.provider = "mistral".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "azure-openai-responses".to_string();
        model.provider = "azure-openai".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "bedrock-converse-stream".to_string();
        model.provider = "amazon-bedrock".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "google-vertex".to_string();
        model.provider = "google-vertex".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "anthropic-messages".to_string();
        model.provider = "anthropic".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "openai-responses".to_string();
        model.provider = "openai".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);

        model.api = "faux".to_string();
        model.provider = "faux".to_string();
        let _prov = AgentSession::default_provider_for_model(&model);
    }

    #[test]
    fn test_set_model_clears_api_key_on_provider_switch_and_preserves_custom_prompt() {
        let dir = std::env::temp_dir().join(format!("agent_session_key_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut anth = faux_model();
        anth.provider = "anthropic".to_string();
        anth.api = "anthropic-messages".to_string();

        let mut session = AgentSession::new_with_trust(
            dir.clone(),
            anth.clone(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        session.set_custom_system_prompt(
            Some("Para 1.\n\nPara 2.".to_string()),
            Some("Tail rule.".to_string()),
        );
        session.set_api_key(Some("sk-ant-secret".to_string()));
        session.enable_mcp_and_codemode(Vec::new(), false);
        assert!(session.system_prompt.contains("Para 1.\n\nPara 2."));
        assert!(session.system_prompt.contains("Tail rule."));

        // Same provider preserves key
        let mut anth2 = anth;
        anth2.id = "claude-opus-4-5".to_string();
        session.set_model(anth2, Arc::new(FauxProvider::new()));
        assert_eq!(session.api_key.as_deref(), Some("sk-ant-secret"));

        // Different provider clears key so Anthropic key cannot leak as OpenAI bearer token
        let mut openai = faux_model();
        openai.provider = "openai".to_string();
        openai.api = "openai-completions".to_string();
        session.set_model(openai, Arc::new(FauxProvider::new()));
        assert!(session.api_key.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_reactive_overflow_auto_compaction_recovers() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_overflow_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut sm = SessionManager::in_memory(&dir);
        for i in 0..4 {
            sm.append_message(Message::User(UserMessage {
                content: UserContent::Text(format!("prior prompt {i}: {}", "data ".repeat(800))),
                timestamp: now_ms(),
            }))
            .unwrap();
            sm.append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                format!("prior reply {i}: {}", "reply ".repeat(800)),
            )))
            .unwrap();
        }

        // Responses consumed FIFO:
        // 1) Initial prompt -> context overflow error
        // 2) Compaction summary response
        // 3) Retry prompt -> final success
        let faux = Arc::new(FauxProvider::with_responses(vec![
            mehh_ai::AssistantMessage {
                content: vec![mehh_ai::AssistantContentBlock::Text(mehh_ai::TextContent {
                    text: String::new(),
                    text_signature: None,
                })],
                api: "faux".to_string(),
                provider: "faux".to_string(),
                model: "faux-model".to_string(),
                response_id: None,
                usage: mehh_ai::Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("prompt is too long: exceeded context length".to_string()),
                timestamp: now_ms(),
            },
            mehh_ai::faux_assistant_message("## Goal\nCompacted summary."),
            mehh_ai::faux_assistant_message("Recovered after compaction!"),
        ]));

        let mut model = faux_model();
        model.context_window = 200_000;
        let mut session = AgentSession::new(
            dir.clone(),
            model,
            faux,
            sm,
            Settings {
                auto_compact: None, // spec default: enabled (true)
                ..Settings::default()
            },
        );

        let msgs = session.prompt("continue", None).await.unwrap();
        let last = msgs.last().unwrap();
        match last {
            Message::Assistant(a) => {
                assert_eq!(a.stop_reason, StopReason::Stop);
                assert_eq!(a.text_content(), "Recovered after compaction!");
            }
            _ => panic!("expected assistant message"),
        }
        assert!(session
            .session
            .entries()
            .iter()
            .any(|e| { matches!(e.kind, crate::session::SessionEntryKind::Compaction { .. }) }));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn navigate_tree_with_summarize_generates_and_appends_branch_summary() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_tree_{}", uuid::Uuid::now_v7()));
        let mut session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );

        // Root entry m0
        let m0 = session
            .session_mut()
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Initial prompt".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Branch A entries: m1, m2
        let _m1 = session
            .session_mut()
            .append_message(Message::Assistant(mehh_ai::faux_assistant_message(
                "Working on branch A",
            )))
            .unwrap();
        let m2 = session
            .session_mut()
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Follow up on branch A".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Switch to m0 without summary (creating branch B)
        session.navigate_tree(&m0, false, None).await.unwrap();
        let m3 = session
            .session_mut()
            .append_message(Message::User(UserMessage {
                content: UserContent::Text("Branch B work".to_string()),
                timestamp: now_ms(),
            }))
            .unwrap();

        // Now from m3, navigate to m2 with summarize = true!
        let summary_opt = session.navigate_tree(&m2, true, None).await.unwrap();
        assert!(summary_opt.is_some());

        // Check active branch entries: leaf should now have branch summary
        let active = session.session().active_path_entries();
        let last_entry = active.last().unwrap();
        match &last_entry.kind {
            SessionEntryKind::BranchSummary { from_id, summary } => {
                assert_eq!(from_id, &m3);
                assert!(!summary.is_empty());
            }
            _ => panic!("Expected BranchSummary as newest entry on active branch"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn prompt_content_forwards_user_blocks_with_image_to_agent_loop() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_blocks_{}", uuid::Uuid::now_v7()));
        let faux = Arc::new(mehh_ai::FauxProvider::with_responses(vec![
            mehh_ai::faux_assistant_message("I see the diagram."),
        ]));
        let mut session = AgentSession::new(
            temp_dir.clone(),
            faux_model(),
            faux.clone(),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
        );

        let blocks = vec![
            mehh_ai::UserContentBlock::Text(mehh_ai::TextContent {
                text: "describe this:".to_string(),
                text_signature: None,
            }),
            mehh_ai::UserContentBlock::Image(mehh_ai::ImageContent {
                mime_type: "image/png".to_string(),
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==".to_string(),
            }),
        ];

        let res = session
            .prompt_content(mehh_ai::UserContent::Blocks(blocks), None)
            .await
            .unwrap();

        assert_eq!(res.len(), 2);
        let active = session.session().active_path_entries();
        let first_msg_entry = active.first().unwrap();
        match &first_msg_entry.kind {
            SessionEntryKind::Message {
                message: Message::User(u),
            } => match &u.content {
                UserContent::Blocks(b) => {
                    assert_eq!(b.len(), 2);
                    assert!(matches!(b[1], mehh_ai::UserContentBlock::Image(_)));
                }
                _ => panic!("Expected UserContent::Blocks"),
            },
            _ => panic!("Expected user message entry"),
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_agent_session_reload_picks_up_new_skill_and_prompt_template_from_disk() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_reload_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mut session = AgentSession::new_with_trust(
            temp_dir.clone(),
            faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
            true,
        );

        assert!(session.skills.is_empty());
        assert!(session.prompt_templates.is_empty());

        // Create a new skill on disk
        let skill_dir = temp_dir.join(".agents").join("skills").join("calc");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: calc\ndescription: Calculates math\n---\nCalc instructions\n",
        )
        .unwrap();

        // Create a prompt template on disk
        let template_dir = temp_dir.join(".mehh").join("prompts");
        std::fs::create_dir_all(&template_dir).unwrap();
        std::fs::write(template_dir.join("review.md"), "Please review this code.").unwrap();

        session.reload();

        assert_eq!(session.skills.len(), 1);
        assert_eq!(session.skills[0].name, "calc");
        assert_eq!(session.prompt_templates.len(), 1);
        assert_eq!(session.prompt_templates[0].name, "review");
        assert!(session.system_prompt.contains("calc"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[derive(Debug)]
    struct MockTool {
        name: String,
    }

    #[async_trait::async_trait]
    impl AgentTool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "mock"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _call_id: &str,
            _params: serde_json::Map<String, serde_json::Value>,
            _cancel: CancellationToken,
            _on_update: Option<mehh_agent::AgentToolUpdateCallback>,
        ) -> Result<mehh_agent::AgentToolResult, String> {
            Ok(mehh_agent::AgentToolResult::text("ok"))
        }
    }

    #[test]
    fn test_sync_mcp_tools_preserves_codemode_and_catalog_handles() {
        let temp_dir =
            std::env::temp_dir().join(format!("agent_session_sync_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mut session = AgentSession::new_with_trust(
            temp_dir.clone(),
            faux_model(),
            Arc::new(mehh_ai::FauxProvider::default()),
            SessionManager::in_memory(&temp_dir),
            Settings::default(),
            true,
        );

        // 1. Codemode preservation
        session.enable_mcp_and_codemode(vec![], true);
        let orig_codemode = session.codemode_tool().expect("codemode must be set");

        // Sync with new mcp tools preserves existing CodemodeTool instance
        session.sync_mcp_tools(vec![], true);
        let synced_codemode = session.codemode_tool().expect("codemode still set");
        assert!(Arc::ptr_eq(&orig_codemode, &synced_codemode));

        // 2. Deferred catalog preservation (> 20 tools)
        let dummy_tools: Vec<Arc<dyn AgentTool>> = (0..25)
            .map(|i| {
                Arc::new(MockTool {
                    name: format!("mcp__srv__tool_{i}"),
                }) as Arc<dyn AgentTool>
            })
            .collect();
        session.sync_mcp_tools(dummy_tools, false);
        let orig_catalog = session.deferred_catalog().expect("catalog must be set");

        let updated_tools: Vec<Arc<dyn AgentTool>> = (0..22)
            .map(|i| {
                Arc::new(MockTool {
                    name: format!("mcp__srv__tool_{i}"),
                }) as Arc<dyn AgentTool>
            })
            .collect();
        session.sync_mcp_tools(updated_tools, false);
        let synced_catalog = session.deferred_catalog().expect("catalog still set");
        assert!(Arc::ptr_eq(&orig_catalog, &synced_catalog));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn prepare_request_replaces_moim_block_instead_of_appending() {
        let dir = std::env::temp_dir().join(format!("agent_session_moim_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        session
            .todo_state
            .write()
            .await
            .add("first task".to_string());

        let (user, _history, config) = session.prepare_prompt("hello");
        let hook = config.prepare_request.expect("prepare hook");
        let mut messages = vec![user];
        for _ in 0..3 {
            let ctx = mehh_agent::types::PrepareRequestContext {
                context: mehh_agent::types::AgentContext {
                    messages: messages.clone(),
                    tools: None,
                },
                model: faux_model(),
                thinking_level: None,
            };
            let update = hook(ctx, CancellationToken::new()).await.expect("update");
            messages = update.context.expect("context").messages;
        }
        let text = match messages.last().expect("user message") {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => t.clone(),
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user message"),
        };
        assert_eq!(
            text.matches("<working_checklist>").count(),
            1,
            "moim block duplicated across turns: {text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cross_provider_worker_uses_worker_key_at_turn_zero() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_worker_key_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        let (manager, lead_key, worker_provider) = {
            let mut mgr = session.lead_worker.lock().unwrap();
            let mut worker = faux_model();
            worker.provider = "openai".to_string();
            mgr.worker_model = Some(worker);
            mgr.set_worker_api_key(Some("worker-value".to_string()));
            mgr.set_lead_api_key(Some("lead-value".to_string()));
            (mgr.clone(), mgr.lead_api_key.clone(), "openai".to_string())
        };
        assert_eq!(
            manager.active_api_key().as_deref(),
            lead_key.as_deref(),
            "turn zero active key is the lead key"
        );
        assert_eq!(
            super::subagent_api_key(&manager, &lead_key, &worker_provider, "anthropic").as_deref(),
            Some("worker-value"),
            "worker tool must never receive the lead key"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn set_model_updates_lead_worker_state() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_lead_model_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        let mut next = faux_model();
        next.id = "next-lead".to_string();
        let next_provider: Arc<dyn LlmProvider> = Arc::new(FauxProvider::new());
        session.set_model(next.clone(), next_provider.clone());
        let lw = session.lead_worker.lock().unwrap();
        assert_eq!(lw.lead_model.id, "next-lead");
        assert!(Arc::ptr_eq(&lw.lead_provider, &next_provider));
        drop(lw);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn after_tool_call_records_lead_worker_outcome() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_outcome_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        let (_user, _history, config) = session.prepare_prompt("hello");
        let hook = config.after_tool_call.expect("after tool hook");
        let ctx = mehh_agent::types::AfterToolCallContext {
            assistant_message: mehh_ai::faux_assistant_message(""),
            tool_call: mehh_ai::ToolCall {
                id: "tc1".to_string(),
                name: "tool".to_string(),
                arguments: serde_json::Map::new(),
                arguments_error: None,
                thought_signature: None,
            },
            args: serde_json::Map::new(),
            result: mehh_agent::AgentToolResult::text("failed"),
            is_error: true,
            context: mehh_agent::types::AgentContext {
                messages: Vec::new(),
                tools: None,
            },
        };
        hook(ctx, CancellationToken::new()).await;
        assert_eq!(
            session.lead_worker.lock().unwrap().consecutive_tool_errors,
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn zero_lead_turns_starts_on_worker_model() {
        let dir =
            std::env::temp_dir().join(format!("agent_session_zero_lead_{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = AgentSession::new_with_trust(
            dir.clone(),
            faux_model(),
            Arc::new(FauxProvider::new()),
            SessionManager::in_memory(&dir),
            Settings::default(),
            false,
        );
        {
            let mut mgr = session.lead_worker.lock().unwrap();
            let mut worker = faux_model();
            worker.id = "worker-first".to_string();
            mgr.worker_model = Some(worker);
            mgr.worker_provider = Some(Arc::new(FauxProvider::new()));
            mgr.lead_turns = 0;
            mgr.set_worker_api_key(Some("worker-value".to_string()));
        }
        let (_user, _history, config) = session.prepare_prompt("hello");
        assert_eq!(config.model.id, "worker-first");
        assert_eq!(
            config.stream_options.api_key.as_deref(),
            Some("worker-value")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
