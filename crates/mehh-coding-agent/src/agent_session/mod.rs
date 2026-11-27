//! Orchestrates an agent session with tool executions, model streaming, and history tracking.

mod compaction;
mod prompt;
mod tools;
#[cfg(test)]
mod tests;

use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use mehh_agent::AgentTool;
use mehh_ai::{
    default_http_client, now_ms, AnthropicProvider, AzureOpenAiResponsesProvider, BedrockProvider,
    FauxProvider, GoogleProvider, LlmProvider, Message, MistralProvider, Model,
    OpenAiCompletionsProvider, OpenAiResponsesProvider, ThinkingLevel, UserContent, UserMessage,
};

use crate::auth::resolve_api_key_async;
use crate::prompt_templates::PromptTemplate;
use crate::session::SessionManager;
use crate::settings::Settings;
use crate::skills::Skill;
use crate::subagent::SubagentTool;
use crate::subagent_control::{SubagentControl, DEFAULT_MAX_CONCURRENT_SUBAGENTS};
use crate::tool_search::DeferredToolCatalog;

/// Returns true when `messages` is a clean turn-0 overflow (`[User, Assistant]`)
/// with no completed turns, tool executions, or drained queue messages.
pub fn can_retry_overflow_from_scratch(messages: &[Message]) -> bool {
    messages.len() == 2
        && matches!(messages[0], Message::User(_))
        && matches!(messages[1], Message::Assistant(_))
}

/// RAII guard for `AgentSession::active_cancel`.
///
/// Unless disarmed, dropping the guard cancels its `CancellationToken` and
/// clears the `active_cancel` slot (if the slot was owned by this guard).
struct ActiveCancelGuard {
    slot: Arc<Mutex<Option<CancellationToken>>>,
    token: CancellationToken,
    armed: bool,
}

impl ActiveCancelGuard {
    /// Installs a fresh token if `slot` is empty (armed), or borrows an existing
    /// outer token (unarmed so an inner call never clears an outer caller's token).
    fn acquire(slot: Arc<Mutex<Option<CancellationToken>>>) -> (CancellationToken, Self) {
        let mut guard = mehh_ai::lock_recover(&slot);
        if let Some(existing) = guard.as_ref() {
            let tok = existing.clone();
            drop(guard);
            return (
                tok.clone(),
                Self {
                    slot,
                    token: tok,
                    armed: false,
                },
            );
        }
        let tok = CancellationToken::new();
        *guard = Some(tok.clone());
        drop(guard);
        (
            tok.clone(),
            Self {
                slot,
                token: tok,
                armed: true,
            },
        )
    }

    /// Unconditionally installs a fresh token into `slot` (armed).
    fn install_fresh(slot: Arc<Mutex<Option<CancellationToken>>>) -> (CancellationToken, Self) {
        let tok = CancellationToken::new();
        *mehh_ai::lock_recover(&slot) = Some(tok.clone());
        (
            tok.clone(),
            Self {
                slot,
                token: tok,
                armed: true,
            },
        )
    }

    /// Replaces the tracked token after `prepare_prompt_content` installs a new loop token.
    fn set_token(&mut self, token: CancellationToken) {
        self.token = token;
        self.armed = true;
    }

    /// Clears the `active_cancel` slot (if armed) and disarms without cancelling the token.
    fn disarm_and_clear(&mut self) {
        if self.armed {
            self.armed = false;
            *mehh_ai::lock_recover(&self.slot) = None;
        }
    }
}

impl Drop for ActiveCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
            *mehh_ai::lock_recover(&self.slot) = None;
        }
    }
}

/// MCP tool count above which `mcp__*` tools are registered behind `tool_search`.
pub const DEFERRED_MCP_TOOL_THRESHOLD: usize = 20;

/// Startup options allowing CLI callers to skip unused default tool, skill, or prompt discovery.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionInitOptions {
    pub skip_default_tools: bool,
    pub skip_skills: bool,
    pub skip_prompt_templates: bool,
    pub no_context_files: bool,
}

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
    pub thinking_explicitly_off: bool,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub no_context_files: bool,
    cached_context_files: Option<Vec<(PathBuf, String)>>,
    pub(crate) cached_prompt_and_tools_tokens: usize,
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
    /// Custom child toolset set by CLI (`--tools subagent`); preserved across
    /// `refresh_subagent_tool` (e.g. `set_approval_handler`, `set_api_key`, `set_model`).
    subagent_child_tools: Option<Vec<Arc<dyn AgentTool>>>,
    /// Explicit tool allowlist recorded by `set_tools` (e.g. CLI `--tools` / `--no-tools`)
    /// so `reload()` never re-injects excluded web tools.
    explicit_tool_allowlist: Option<std::collections::HashSet<String>>,
    /// Explicit tool allowlist recorded by `set_subagent_with_child_tools`.
    subagent_child_tool_allowlist: Option<std::collections::HashSet<String>>,
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

/// Normalizes a requested thinking level against `model`'s capability profile.
/// Returns `(effective_ui_level, send_explicit_off_to_provider)`.
pub fn normalize_thinking_for_model(
    model: &Model,
    requested: Option<ThinkingLevel>,
    explicitly_off: bool,
) -> (Option<ThinkingLevel>, bool) {
    let profile = mehh_ai::model_thinking_profile(model);
    if !profile.supported {
        return (None, false);
    }

    if requested == Some(ThinkingLevel::Off) || (requested.is_none() && explicitly_off) {
        if profile.can_disable {
            return (None, model.reasoning);
        }
        return (
            if profile.default_level == ThinkingLevel::Off {
                None
            } else {
                Some(profile.default_level)
            },
            false,
        );
    }

    if let Some(lvl) = requested {
        if model.provider.eq_ignore_ascii_case("faux") || profile.levels.contains(&lvl) {
            return (Some(lvl), false);
        }
        let mapped = match lvl {
            ThinkingLevel::Max | ThinkingLevel::Xhigh => {
                if profile.levels.contains(&ThinkingLevel::High) {
                    ThinkingLevel::High
                } else {
                    profile.default_level
                }
            }
            ThinkingLevel::Minimal => {
                if profile.levels.contains(&ThinkingLevel::Low) {
                    ThinkingLevel::Low
                } else {
                    profile.default_level
                }
            }
            _ => profile.default_level,
        };
        return (
            if mapped == ThinkingLevel::Off {
                None
            } else {
                Some(mapped)
            },
            false,
        );
    }

    (
        if profile.default_level == ThinkingLevel::Off {
            None
        } else {
            Some(profile.default_level)
        },
        false,
    )
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
        Self::new_with_options(
            cwd,
            model,
            provider,
            session,
            settings,
            trusted,
            SessionInitOptions::default(),
        )
    }

    /// Creates a new agent session with explicit startup options to avoid redundant tool,
    /// skill, prompt template, or context-file work that CLI flags will immediately override.
    pub fn new_with_options(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        session: SessionManager,
        settings: Settings,
        trusted: bool,
        options: SessionInitOptions,
    ) -> Self {
        let skills = if options.skip_skills {
            Vec::new()
        } else {
            crate::skills::load_skills_with_trust(&cwd, trusted)
        };
        let prompt_templates = if options.skip_prompt_templates {
            Vec::new()
        } else {
            crate::prompt_templates::load_prompt_templates_with_trust(&cwd, trusted)
        };

        let thinking_explicitly_off = settings.default_thinking_level == Some(ThinkingLevel::Off);
        let thinking_level = match settings.default_thinking_level {
            Some(ThinkingLevel::Off) => None,
            other => other,
        };
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

        let subagent_control = Arc::new(SubagentControl::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS));
        let todo_state = Arc::new(tokio::sync::RwLock::new(crate::todo::TodoState::default()));

        let tools = if options.skip_default_tools {
            Vec::new()
        } else {
            let web_config = settings
                .to_web_config()
                .with_url_policy_checker(inspection_manager.clone());
            let mut base_tools = mehh_tools::create_coding_tools_with_web(cwd.clone(), web_config);

            let (subagent_model, subagent_provider) = {
                let lw = mehh_ai::lock_recover(&lead_worker);
                (
                    lw.worker_model()
                        .cloned()
                        .unwrap_or_else(|| model.clone()),
                    lw.worker_provider()
                        .cloned()
                        .unwrap_or_else(|| provider.clone()),
                )
            };
            let subagent_key = if subagent_model.provider.eq_ignore_ascii_case(&model.provider) {
                None
            } else {
                worker_api_key
            };
            let subagent = Arc::new(
                SubagentTool::new(
                    cwd.clone(),
                    subagent_model,
                    subagent_provider,
                    subagent_key,
                    base_tools.clone(),
                    0,
                )
                .with_inspection_manager(inspection_manager.clone())
                .with_control(subagent_control.clone())
                .with_lead_worker(lead_worker.clone()),
            );
            base_tools.push(subagent);
            let todo_tool = Arc::new(crate::todo::TodoTool::new(todo_state.clone()));
            base_tools.push(todo_tool);
            base_tools
        };

        let cached_context_files = if trusted && !options.no_context_files {
            Some(crate::system_prompt::discover_context_files(&cwd))
        } else {
            None
        };
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let system_prompt = crate::system_prompt::build_system_prompt_with_skills_and_preloaded_context(
            &cwd,
            None,
            None,
            &tool_names,
            &skills,
            cached_context_files.as_deref().unwrap_or(&[]),
        );
        let cached_prompt_and_tools_tokens = system_prompt.chars().count().div_ceil(4)
            + crate::compaction::estimate_tool_definitions_tokens(&tools);

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
            thinking_explicitly_off,
            skills,
            prompt_templates,
            no_context_files: options.no_context_files,
            cached_context_files,
            cached_prompt_and_tools_tokens,
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
            subagent_child_tools: None,
            explicit_tool_allowlist: None,
            subagent_child_tool_allowlist: None,
        }
    }

    /// Queues a steering user message to be processed at the next turn or tool call boundary.
    pub fn steer(&self, text: impl Into<String>) {
        let msg = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        mehh_ai::lock_recover(&self.steering_queue).push_back(msg);
    }

    /// Queues a follow-up user message to be processed once the current turn completes.
    pub fn follow_up(&self, text: impl Into<String>) {
        let msg = Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_ms(),
        });
        mehh_ai::lock_recover(&self.follow_up_queue).push_back(msg);
    }

    /// Returns the number of pending steering messages.
    pub fn pending_steering_count(&self) -> usize {
        mehh_ai::lock_recover(&self.steering_queue).len()
    }

    /// Returns the number of pending follow-up messages.
    pub fn pending_follow_up_count(&self) -> usize {
        mehh_ai::lock_recover(&self.follow_up_queue).len()
    }

    /// Clears both steering and follow-up queues.
    pub fn clear_queues(&self) {
        mehh_ai::lock_recover(&self.steering_queue).clear();
        mehh_ai::lock_recover(&self.follow_up_queue).clear();
    }

    /// Aborts any currently active running prompt loop. Returns true if a cancel was triggered.
    ///
    /// The cancelled token stays in the slot. That lets the next turn's
    /// `prepare_prompt_content` inherit the cancellation when Ctrl+C lands in
    /// the handoff window after the UI already checked the preparation token.
    pub fn abort(&self) -> bool {
        let guard = mehh_ai::lock_recover(&self.active_cancel);
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
        mehh_ai::lock_recover(&self.active_cancel)
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
        self.sync_thinking_for_model(true);
        Ok(Some(next_model))
    }

    /// Aborts any currently running bash tool command.
    pub fn abort_bash(&self) {
        if let Some(tok) = mehh_ai::lock_recover(&self.active_bash_cancel).take() {
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
        *mehh_ai::write_recover(&self.approval_handler_slot) = self.approval_handler.clone();
        self.refresh_subagent_tool();
    }

    /// Overrides active tools for the session.
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) {
        self.explicit_tool_allowlist = Some(tools.iter().map(|t| t.name().to_string()).collect());
        self.tools = tools;
        self.subagent_child_tools = None;
        self.subagent_child_tool_allowlist = None;
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
        if self.trusted && !self.no_context_files {
            if self.cached_context_files.is_none() {
                self.cached_context_files =
                    Some(crate::system_prompt::discover_context_files(&self.cwd));
            }
        } else {
            self.cached_context_files = None;
        }
        self.system_prompt =
            crate::system_prompt::build_system_prompt_with_skills_and_preloaded_context(
                &self.cwd,
                self.custom_system_prompt.as_deref(),
                self.append_system_prompt.as_deref(),
                &tool_names,
                &self.skills,
                self.cached_context_files.as_deref().unwrap_or(&[]),
            );
        self.cached_prompt_and_tools_tokens = self.system_prompt.chars().count().div_ceil(4)
            + crate::compaction::estimate_tool_definitions_tokens(&self.tools);
    }

    /// Skips loading AGENTS.md / CLAUDE.md into the system prompt.
    pub fn set_no_context_files(&mut self, value: bool) {
        if self.no_context_files == value {
            return;
        }
        self.no_context_files = value;
        if value {
            self.cached_context_files = None;
        }
        self.rebuild_system_prompt();
    }

    /// Reloads settings, permissions, tools, skills, and prompt templates from disk,
    /// then rebuilds inspection state and the system prompt.
    pub fn reload(&mut self) -> Result<()> {
        let settings =
            crate::settings::SettingsManager::try_load_with_trust(&self.cwd, self.trusted)?;
        let worker_registry = if settings.worker_model.is_some() {
            Some(crate::models_config::ModelRegistry::load_default()?)
        } else {
            None
        };

        self.settings = settings;
        self.skills = crate::skills::load_skills_with_trust(&self.cwd, self.trusted);
        self.prompt_templates =
            crate::prompt_templates::load_prompt_templates_with_trust(&self.cwd, self.trusted);
        self.cached_context_files = None;

        let default_mode = if self.model.provider == "faux" || self.model.api == "faux" {
            crate::tool_inspection::PermissionMode::Auto
        } else {
            crate::tool_inspection::PermissionMode::SmartApprove
        };
        let prev_session_rules = self.inspection_manager.try_snapshot_session_rules();
        let prev_mode = self.inspection_manager.try_mode();
        let permission_mode = match self.settings.permission_mode {
            Some(disk_mode) => disk_mode,
            None => match prev_mode {
                Some(prev)
                    if crate::settings::permission_mode_rank(prev)
                        < crate::settings::permission_mode_rank(default_mode) =>
                {
                    prev
                }
                _ => default_mode,
            },
        };
        let mut perm_store =
            crate::permissions::PermissionStore::load_fail_closed(&self.cwd, self.trusted);
        perm_store.restore_session_rules(prev_session_rules);
        self.inspection_manager = Arc::new(
            crate::tool_inspection::ToolInspectionManager::new_with_store(
                permission_mode,
                self.cwd.clone(),
                perm_store,
            ),
        );

        {
            let mut lw = mehh_ai::lock_recover(&self.lead_worker);
            lw.lead_turns = self.settings.lead_turns.unwrap_or(1);
            if let (Some(wm), Some(reg)) = (
                self.settings.worker_model.as_deref(),
                worker_registry.as_ref(),
            ) {
                if let Some(m) = reg.resolve(wm, None) {
                    let p = Self::default_provider_for_model(&m);
                    let key = if m.provider == "faux" || m.api == "faux" {
                        None
                    } else {
                        crate::auth::resolve_api_key(&m.provider, None, None)
                    };
                    lw.worker_model = Some(m);
                    lw.worker_provider = Some(p);
                    lw.worker_api_key = key;
                } else {
                    lw.worker_model = None;
                    lw.worker_provider = None;
                    lw.worker_api_key = None;
                }
            } else {
                lw.worker_model = None;
                lw.worker_provider = None;
                lw.worker_api_key = None;
            }
        }

        // Re-sync web tools with updated WebConfig and new inspection_manager URL checker,
        // preserving explicit CLI tool allowlists so excluded tools are never re-injected.
        let web_config = self
            .settings
            .to_web_config()
            .with_url_policy_checker(self.inspection_manager.clone());
        let fresh_web_tools = mehh_tools::create_web_tools(web_config);

        let has_core_tools = |tools: &[Arc<dyn AgentTool>]| -> bool {
            tools.iter().any(|t| {
                matches!(
                    t.name(),
                    "read" | "write" | "edit" | "bash" | "grep" | "find" | "ls" | "analyze" | "web_search" | "web_fetch"
                )
            })
        };

        let sync_web = |tools: &mut Vec<Arc<dyn AgentTool>>,
                        allowlist: Option<&std::collections::HashSet<String>>| {
            let had_core = has_core_tools(tools);
            tools.retain(|t| !matches!(t.name(), "web_search" | "web_fetch"));
            let allowed: Vec<Arc<dyn AgentTool>> = fresh_web_tools
                .iter()
                .filter(|wt| allowlist.is_none_or(|set| set.contains(wt.name())))
                .cloned()
                .collect();
            let should_insert = match allowlist {
                Some(_) => !allowed.is_empty(),
                None => had_core && !allowed.is_empty(),
            };
            if should_insert {
                let insert_pos = tools
                    .iter()
                    .position(|t| matches!(t.name(), "subagent" | "todo" | "codemode"))
                    .unwrap_or(tools.len());
                for (offset, wt) in allowed.iter().enumerate() {
                    tools.insert(insert_pos + offset, Arc::clone(wt));
                }
            }
        };

        sync_web(&mut self.tools, self.explicit_tool_allowlist.as_ref());
        if let Some(ref mut child_tools) = self.subagent_child_tools {
            sync_web(child_tools, self.subagent_child_tool_allowlist.as_ref());
        }

        if self.codemode_tool.is_some() {
            self.tools.retain(|t| t.name() != "codemode");
            let new_codemode = self.create_codemode_tool(self.tools.clone());
            self.codemode_tool = Some(new_codemode.clone());
            self.tools.push(new_codemode);
        }

        self.refresh_subagent_tool();
        self.rebuild_system_prompt();
        Ok(())
    }


    /// Sets or updates the system prompt directly.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
        self.cached_prompt_and_tools_tokens = self.system_prompt.chars().count().div_ceil(4)
            + crate::compaction::estimate_tool_definitions_tokens(&self.tools);
    }

    /// Sets or updates the provider API key (clearing any explicit CLI override marker).
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = false;
        self.api_key = api_key.clone();
        mehh_ai::lock_recover(&self.lead_worker).set_lead_api_key(api_key);
        self.refresh_subagent_tool();
    }

    /// Clears in-memory credentials for `provider` from both lead and worker state if matched.
    pub fn clear_provider_api_keys(&mut self, provider: &str) -> bool {
        let mut cleared = false;
        if self.model.provider.eq_ignore_ascii_case(provider) {
            if self.api_key.is_some() || self.has_explicit_api_key {
                cleared = true;
            }
            self.api_key = None;
            self.has_explicit_api_key = false;
            mehh_ai::lock_recover(&self.lead_worker).set_lead_api_key(None);
        }
        {
            let mut mgr = mehh_ai::lock_recover(&self.lead_worker);
            if mgr
                .worker_model()
                .is_some_and(|w| w.provider.eq_ignore_ascii_case(provider))
                && mgr.worker_api_key.is_some()
            {
                mgr.set_worker_api_key(None);
                cleared = true;
            }
        }
        if cleared {
            self.refresh_subagent_tool();
        }
        cleared
    }

    /// Sets an explicit CLI `--api-key` override that must not be clobbered on same-provider switches.
    pub fn set_explicit_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = api_key.as_deref().is_some_and(|k| !k.trim().is_empty());
        self.api_key = api_key.clone();
        mehh_ai::lock_recover(&self.lead_worker).set_lead_api_key(api_key);
        self.refresh_subagent_tool();
    }

    /// Returns whether the active key was explicitly pinned by CLI `--api-key`.
    pub fn has_explicit_api_key(&self) -> bool {
        self.has_explicit_api_key
    }

    /// Sets or updates reasoning/thinking level, normalizing against the active model's profile.
    pub fn set_thinking_level(&mut self, level: Option<ThinkingLevel>) {
        match level {
            None | Some(ThinkingLevel::Off) => {
                self.thinking_explicitly_off = true;
                self.thinking_level = None;
            }
            Some(lvl) => {
                self.thinking_explicitly_off = false;
                self.thinking_level = Some(lvl);
            }
        }
        self.sync_thinking_for_model(true);
    }

    /// Updates both model metadata and its backing provider implementation.
    /// Clears `api_key` whenever switching to a different provider so keys never leak across providers.
    pub fn set_model(&mut self, model: Model, provider: Arc<dyn LlmProvider>) {
        if !self.model.provider.eq_ignore_ascii_case(&model.provider) {
            self.api_key = None;
            self.has_explicit_api_key = false;
            mehh_ai::lock_recover(&self.lead_worker).set_lead_api_key(None);
        }
        self.model = model;
        self.provider = provider;
        {
            let mut mgr = mehh_ai::lock_recover(&self.lead_worker);
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

    /// Returns the effective normalized thinking/reasoning level for the active model.
    pub fn effective_thinking_level(&self) -> Option<ThinkingLevel> {
        normalize_thinking_for_model(&self.model, self.thinking_level, self.thinking_explicitly_off)
            .0
    }

    /// Sets the thinking level explicitly from user action, tracking explicit Off choices.
    pub fn set_thinking_level_explicit(&mut self, level: ThinkingLevel) {
        self.set_thinking_level(Some(level));
    }

    /// Synchronizes `thinking_level` with the active model's thinking capability profile.
    pub fn sync_thinking_for_model(&mut self, _preserve_existing: bool) {
        self.thinking_level = self.effective_thinking_level();
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
        match crate::models_config::lookup_provider_api_key(provider, None) {
            Ok(key) => Ok(key),
            Err(err) if crate::user_home_dir().is_none() => {
                let _ = err;
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    /// Resolves and refreshes `api_key` asynchronously for both Lead and Worker models
    /// (refreshing expiring OAuth tokens across turns).
    pub async fn ensure_api_key(&mut self, auth_file: Option<&std::path::Path>) -> Result<()> {
        let mut changed = false;
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
                mehh_ai::lock_recover(&self.lead_worker).set_lead_api_key(self.api_key.clone());
                changed = true;
            }
        }

        let worker_info = {
            let mgr = mehh_ai::lock_recover(&self.lead_worker);
            mgr.worker_model()
                .map(|w| (w.provider.clone(), w.api.clone()))
        };
        if let Some((worker_provider, worker_api)) = worker_info {
            if worker_provider.eq_ignore_ascii_case(&self.model.provider) {
                if let Some(ref lead_key) = self.api_key {
                    let mut mgr = mehh_ai::lock_recover(&self.lead_worker);
                    if mgr.worker_api_key.as_deref() != Some(lead_key.as_str())
                        && (self.has_explicit_api_key || mgr.worker_api_key.is_some())
                    {
                        mgr.set_worker_api_key(Some(lead_key.clone()));
                        changed = true;
                    }
                }
            } else if let Some(w_key) =
                Self::refresh_resolved_api_key(&worker_provider, &worker_api, false, auth_file)
                    .await?
            {
                let mut mgr = mehh_ai::lock_recover(&self.lead_worker);
                if mgr.worker_api_key.as_deref() != Some(w_key.as_str()) {
                    mgr.set_worker_api_key(Some(w_key));
                    changed = true;
                }
            }
        }

        if changed {
            self.refresh_subagent_tool();
        }
        Ok(())
    }
}
