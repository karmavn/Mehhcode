//! Orchestrates an agent session with tool executions, model streaming, and history tracking.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use anyhow::Result;
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
use crate::system_prompt::build_system_prompt_with_skills;
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
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
    pub follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    pub active_cancel: Arc<Mutex<Option<CancellationToken>>>,
    dynamic_tools: Arc<RwLock<Vec<Arc<dyn AgentTool>>>>,
    deferred_catalog: Option<Arc<DeferredToolCatalog>>,
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
        let subagent = Arc::new(SubagentTool::new(
            cwd.clone(),
            model.clone(),
            provider.clone(),
            None,
            tools.clone(),
            0,
        ));
        tools.push(subagent);
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let system_prompt =
            build_system_prompt_with_skills(&cwd, None, None, &tool_names, &skills);
        let thinking_level = settings.default_thinking_level;

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
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            active_cancel: Arc::new(Mutex::new(None)),
            dynamic_tools: Arc::new(RwLock::new(Vec::new())),
            deferred_catalog: None,
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
    pub fn abort(&self) -> bool {
        let cancel = self.active_cancel.lock().unwrap().take();
        if let Some(c) = cancel {
            c.cancel();
            true
        } else {
            false
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

    /// Returns the shared steering queue.
    pub fn steering_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.steering_queue.clone()
    }

    /// Returns the shared follow-up queue.
    pub fn follow_up_queue(&self) -> Arc<Mutex<VecDeque<Message>>> {
        self.follow_up_queue.clone()
    }

    /// Returns the slot holding the active cancellation token.
    pub fn active_cancel_slot(&self) -> Arc<Mutex<Option<CancellationToken>>> {
        self.active_cancel.clone()
    }

    /// Overrides active tools for the session.
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) {
        self.tools = tools;
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
        self.system_prompt = build_system_prompt_with_skills(
            &self.cwd,
            self.custom_system_prompt.as_deref(),
            self.append_system_prompt.as_deref(),
            &tool_names,
            &self.skills,
        );
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
        } else {
            self.tools.extend(mcp_tools);
            if enable_codemode {
                let codemode_tool = Arc::new(mehh_codemode::CodemodeTool::new(self.tools.clone()));
                self.tools.push(codemode_tool);
            }
            self.rebuild_system_prompt();
        }
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
        let catalog = Arc::new(DeferredToolCatalog::new(deferred));
        self.dynamic_tools = catalog.dynamic_tools_handle();
        self.deferred_catalog = Some(Arc::clone(&catalog));
        self.tools.retain(|t| t.name() != "tool_search");
        self.tools.push(Arc::new(ToolSearchTool::new(catalog)));
        self.rebuild_system_prompt();
    }

    fn refresh_subagent_tool(&mut self) {
        let base_tools: Vec<Arc<dyn AgentTool>> = self
            .tools
            .iter()
            .filter(|t| t.name() != "subagent" && t.name() != "codemode")
            .cloned()
            .collect();
        let updated = Arc::new(SubagentTool::new(
            self.cwd.clone(),
            self.model.clone(),
            self.provider.clone(),
            self.api_key.clone(),
            base_tools,
            0,
        ));
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == "subagent") {
            *slot = updated;
        }
    }

    /// Sets or updates the system prompt directly.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
    }

    /// Sets or updates the provider API key (clearing any explicit CLI override marker).
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = false;
        self.api_key = api_key;
        self.refresh_subagent_tool();
    }

    /// Sets an explicit CLI `--api-key` override that must not be clobbered on same-provider switches.
    pub fn set_explicit_api_key(&mut self, api_key: Option<String>) {
        self.has_explicit_api_key = api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        self.api_key = api_key;
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
        }
        self.model = model;
        self.provider = provider;
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

    /// Prepares input, history, and agent loop configuration for a prompt turn.
    pub fn prepare_prompt(&self, text: &str) -> (Message, Vec<Message>, AgentLoopConfig) {
        let expanded = self.expand_input(text);
        let user_msg = Message::User(UserMessage {
            content: UserContent::Text(expanded),
            timestamp: now_ms(),
        });

        let history = self.session.build_context_messages();

        let cancel = CancellationToken::new();
        *self.active_cancel.lock().unwrap() = Some(cancel.clone());

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
        config.stream_options.reasoning = self.thinking_level;
        if let Some(t) = self.settings.temperature {
            config.stream_options.temperature = Some(t);
        }
        if let Some(m) = self.settings.max_tokens {
            config.stream_options.max_tokens = Some(m);
        }

        (user_msg, history, config)
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
            crate::compaction::estimate_context_tokens(&self.session.build_context_messages());
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

    /// Sends a user prompt through the agent loop, expanding skills or prompt templates,
    /// performing proactive and reactive overflow auto-compaction (when enabled, default true),
    /// and persisting messages to the session.
    pub async fn prompt(
        &mut self,
        text: &str,
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

        let (user_msg, history, config) = self.prepare_prompt(text);
        let new_messages = run_agent_loop(vec![user_msg], history, config, on_event.clone()).await;
        *self.active_cancel.lock().unwrap() = None;

        if self.is_overflow_turn(&new_messages) {
            match self.try_overflow_compaction().await {
                Ok(true) => {
                    let (retry_user, retry_history, retry_config) = self.prepare_prompt(text);
                    let retry_messages =
                        run_agent_loop(vec![retry_user], retry_history, retry_config, on_event)
                            .await;
                    *self.active_cancel.lock().unwrap() = None;
                    self.finish_prompt(&retry_messages)?;
                    return Ok(retry_messages);
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!("Warning: overflow auto-compaction failed: {e}");
                }
            }
        }

        self.finish_prompt(&new_messages)?;
        Ok(new_messages)
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

        let prep = match crate::compaction::prepare_compaction(&active_entries, keep_recent_tokens) {
            Some(p) => p,
            None => return Ok(None),
        };
        let summary = self.run_prepared_compaction(prep, custom_instructions).await?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::{faux_model, StopReason};

    #[test]
    fn test_queues_and_abort() {
        let temp_dir = std::env::temp_dir().join(format!("agent_session_q_{}", uuid::Uuid::now_v7()));
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
            sm.append_message(Message::Assistant(mehh_ai::faux_assistant_message(&format!(
                "prior reply {i}: {}",
                "reply ".repeat(800)
            ))))
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
        assert!(session.session.entries().iter().any(|e| {
            matches!(e.kind, crate::session::SessionEntryKind::Compaction { .. })
        }));

        let _ = std::fs::remove_dir_all(dir);
    }
}
