use std::sync::Arc;
use mehh_ai::{faux_model, FauxProvider, LlmProvider, Model, StopReason};

/// Default number of Lead turns granted per rescue before returning to Worker.
pub const DEFAULT_LEAD_RESCUE_TURNS: usize = 2;

/// Manages dual-model routing between a powerful reasoning Lead model
/// and an efficient execution Worker model with cross-provider credential isolation.
///
/// Escalation is temporary: a failing Worker turn grants the Lead a bounded
/// rescue window, after which routing returns to the Worker. Only repeated
/// escalations (more than two in one prompt) lock routing to the Lead.
#[derive(Clone)]
pub struct LeadWorkerManager {
    pub lead_model: Model,
    pub worker_model: Option<Model>,
    pub lead_turns: usize,
    pub current_turn_in_prompt: usize,
    /// Lead turns still owed to the current rescue window (0 = Worker routing).
    pub lead_rescue_remaining: usize,
    /// How many times the Worker has escalated in this prompt.
    pub escalation_count: u8,
    /// Set once escalations exceed two: stay on Lead for the rest of the prompt.
    pub locked_to_lead: bool,
    /// Consecutive failed tool executions on the current Worker turn.
    pub consecutive_tool_errors: u8,
    /// Lead turns granted per rescue window.
    pub rescue_turns: usize,
    pub lead_provider: Arc<dyn LlmProvider>,
    pub worker_provider: Option<Arc<dyn LlmProvider>>,
    pub lead_api_key: Option<String>,
    pub worker_api_key: Option<String>,
}

impl LeadWorkerManager {
    pub fn new(
        lead_model: Model,
        lead_provider: Arc<dyn LlmProvider>,
        worker_model: Option<Model>,
        worker_provider: Option<Arc<dyn LlmProvider>>,
        lead_turns: usize,
    ) -> Self {
        Self {
            lead_model,
            worker_model,
            lead_turns,
            current_turn_in_prompt: 0,
            lead_rescue_remaining: 0,
            escalation_count: 0,
            locked_to_lead: false,
            consecutive_tool_errors: 0,
            rescue_turns: DEFAULT_LEAD_RESCUE_TURNS,
            lead_provider,
            worker_provider,
            lead_api_key: None,
            worker_api_key: None,
        }
    }

    pub fn new_for_test(lead_name: &str, worker_name: &str, lead_turns: usize) -> Self {
        let mut lead = faux_model();
        lead.id = lead_name.to_string();
        let mut worker = faux_model();
        worker.id = worker_name.to_string();

        let faux_lead = Arc::new(FauxProvider::default());
        let faux_worker = Arc::new(FauxProvider::default());

        Self::new(
            lead,
            faux_lead,
            Some(worker),
            Some(faux_worker),
            lead_turns,
        )
    }

    pub fn is_active(&self) -> bool {
        self.worker_model.is_some()
    }

    pub fn worker_model(&self) -> Option<&Model> {
        self.worker_model.as_ref()
    }

    pub fn worker_provider(&self) -> Option<&Arc<dyn LlmProvider>> {
        self.worker_provider.as_ref()
    }

    pub fn active_model_for_turn(&self, turn: usize) -> &str {
        if !self.locked_to_lead
            && self.lead_rescue_remaining == 0
            && turn >= self.lead_turns
        {
            if let Some(ref worker) = self.worker_model {
                return &worker.id;
            }
        }
        &self.lead_model.id
    }

    pub fn should_use_worker(&self) -> bool {
        !self.locked_to_lead
            && self.lead_rescue_remaining == 0
            && self.worker_model.is_some()
            && self.current_turn_in_prompt >= self.lead_turns
    }

    pub fn active_model(&self) -> Model {
        if self.should_use_worker() {
            if let Some(ref worker) = self.worker_model {
                return worker.clone();
            }
        }
        self.lead_model.clone()
    }

    pub fn active_provider(&self) -> Arc<dyn LlmProvider> {
        if self.should_use_worker() {
            if let Some(ref worker_prov) = self.worker_provider {
                return worker_prov.clone();
            }
        }
        self.lead_provider.clone()
    }

    pub fn active_api_key(&self) -> Option<String> {
        if self.should_use_worker() {
            self.worker_api_key.clone()
        } else {
            self.lead_api_key.clone()
        }
    }

    pub fn set_lead_api_key(&mut self, key: Option<String>) {
        self.lead_api_key = key;
    }

    pub fn set_worker_api_key(&mut self, key: Option<String>) {
        self.worker_api_key = key;
    }

    /// Records one tool execution outcome for the consecutive-error trigger.
    pub fn record_tool_outcome(&mut self, is_error: bool) {
        if is_error {
            self.consecutive_tool_errors = self.consecutive_tool_errors.saturating_add(1);
        } else {
            self.consecutive_tool_errors = 0;
        }
    }

    pub fn record_turn_outcome(&mut self, stop_reason: StopReason, is_repetition_break: bool) {
        let was_worker_turn = self.should_use_worker();
        if was_worker_turn
            && (stop_reason == StopReason::Error
                || is_repetition_break
                || self.consecutive_tool_errors >= 2)
        {
            self.escalation_count = self.escalation_count.saturating_add(1);
            if self.escalation_count > 2 {
                self.locked_to_lead = true;
            } else {
                self.lead_rescue_remaining = self.rescue_turns;
            }
        } else if self.lead_rescue_remaining > 0 {
            self.lead_rescue_remaining -= 1;
        }
        self.consecutive_tool_errors = 0;
        self.current_turn_in_prompt += 1;
    }

    pub fn reset_for_new_prompt(&mut self) {
        self.current_turn_in_prompt = 0;
        self.lead_rescue_remaining = 0;
        self.escalation_count = 0;
        self.locked_to_lead = false;
        self.consecutive_tool_errors = 0;
    }
}
