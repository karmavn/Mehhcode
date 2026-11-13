//! Orchestrates an agent session with tool executions, model streaming, and history tracking.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use mehh_agent::{run_agent_loop, AgentEvent, AgentLoopConfig, AgentTool};
use mehh_ai::{
    now_ms, AnthropicProvider, AzureOpenAiResponsesProvider, BedrockProvider, FauxProvider,
    GoogleProvider, LlmProvider, Message, MistralProvider, Model, OpenAiCompletionsProvider,
    OpenAiResponsesProvider, ThinkingLevel, UserContent, UserMessage,
};

use crate::auth::resolve_api_key_async;
use crate::prompt_templates::PromptTemplate;
use crate::session::SessionManager;
use crate::settings::Settings;
use crate::skills::Skill;
use crate::system_prompt::build_system_prompt_with_skills;

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
    pub api_key: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub steering_queue: Arc<Mutex<VecDeque<Message>>>,
    pub follow_up_queue: Arc<Mutex<VecDeque<Message>>>,
    pub active_cancel: Arc<Mutex<Option<CancellationToken>>>,
}

impl AgentSession {
    /// Creates a new agent session, initializing default tools, skills, templates, and structured system prompt.
    pub fn new(
        cwd: PathBuf,
        model: Model,
        provider: Arc<dyn LlmProvider>,
        session: SessionManager,
        settings: Settings,
    ) -> Self {
        let skills = crate::skills::load_skills(&cwd);
        let prompt_templates = crate::prompt_templates::load_prompt_templates(&cwd);
        let tools = mehh_tools::create_coding_tools(cwd.clone());
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
            api_key: None,
            thinking_level,
            skills,
            prompt_templates,
            steering_queue: Arc::new(Mutex::new(VecDeque::new())),
            follow_up_queue: Arc::new(Mutex::new(VecDeque::new())),
            active_cancel: Arc::new(Mutex::new(None)),
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

    /// Injects MCP tools and optionally wraps all tools in CodemodeTool,
    /// rebuilding the system prompt to include the newly available tools.
    pub fn enable_mcp_and_codemode(
        &mut self,
        mcp_tools: Vec<Arc<dyn AgentTool>>,
        enable_codemode: bool,
    ) {
        self.tools.extend(mcp_tools);
        if enable_codemode {
            let codemode_tool = Arc::new(mehh_codemode::CodemodeTool::new(self.tools.clone()));
            self.tools.push(codemode_tool);
        }
        let tool_names: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
        self.system_prompt = build_system_prompt_with_skills(
            &self.cwd,
            None,
            None,
            &tool_names,
            &self.skills,
        );
    }

    /// Sets or updates the system prompt.
    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
    }

    /// Sets or updates the provider API key.
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.api_key = api_key;
    }

    /// Sets or updates reasoning/thinking level.
    pub fn set_thinking_level(&mut self, level: Option<ThinkingLevel>) {
        self.thinking_level = level;
    }

    /// Updates both model metadata and its backing provider implementation.
    pub fn set_model(&mut self, model: Model, provider: Arc<dyn LlmProvider>) {
        self.model = model;
        self.provider = provider;
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
            Arc::new(AnthropicProvider::new(reqwest::Client::new()))
        } else if model.api == "openai-responses" {
            Arc::new(OpenAiResponsesProvider::new())
        } else {
            Arc::new(OpenAiCompletionsProvider::new(reqwest::Client::new()))
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

    /// Sends a user prompt through the agent loop, expanding skills or prompt templates,
    /// and persisting messages to the session.
    pub async fn prompt(
        &mut self,
        text: &str,
        on_event: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    ) -> Result<Vec<Message>> {
        // Resolve API key asynchronously if absent and not running faux
        if self.api_key.is_none() && self.model.provider != "faux" && self.model.api != "faux" {
            if let Ok(Some(key)) = resolve_api_key_async(&self.model.provider, None, None).await {
                self.api_key = Some(key);
            }
        }

        let (user_msg, history, config) = self.prepare_prompt(text);
        let new_messages = run_agent_loop(vec![user_msg], history, config, on_event).await;
        *self.active_cancel.lock().unwrap() = None;
        self.finish_prompt(&new_messages)?;
        Ok(new_messages)
    }

    /// Manually triggers compaction on the active branch history, saving the summary entry.
    pub async fn compact(&mut self, custom_instructions: Option<&str>) -> Result<Option<String>> {
        let active_entries: Vec<crate::session::SessionEntry> = self
            .session
            .active_path_entries()
            .into_iter()
            .cloned()
            .collect();

        let prep = match crate::compaction::prepare_compaction(
            &active_entries,
            crate::compaction::DEFAULT_KEEP_RECENT_TOKENS,
        ) {
            Some(p) => p,
            None => return Ok(None),
        };

        let cancel = tokio_util::sync::CancellationToken::new();
        let summary = crate::compaction::compact_with_provider(
            &prep,
            &self.model,
            self.provider.as_ref(),
            self.api_key.as_deref(),
            custom_instructions,
            cancel,
        )
        .await?;

        self.session.append_entry(crate::session::SessionEntryKind::Compaction {
            summary: summary.clone(),
            first_kept_entry_id: prep.first_kept_entry_id,
            tokens_before: prep.tokens_before,
        })?;

        Ok(Some(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mehh_ai::faux_model;

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
}
