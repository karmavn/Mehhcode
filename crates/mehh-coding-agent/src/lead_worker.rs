use std::sync::Arc;
use mehh_ai::{faux_model, FauxProvider, LlmProvider, Model, StopReason};

/// Manages dual-model routing between a powerful reasoning Lead model
/// and an efficient execution Worker model with cross-provider credential isolation.
#[derive(Clone)]
pub struct LeadWorkerManager {
    pub lead_model: Model,
    pub worker_model: Option<Model>,
    pub lead_turns: usize,
    pub current_turn_in_prompt: usize,
    pub escalated_to_lead: bool,
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
            escalated_to_lead: false,
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
        if !self.escalated_to_lead && turn >= self.lead_turns {
            if let Some(ref worker) = self.worker_model {
                return &worker.id;
            }
        }
        &self.lead_model.id
    }

    pub fn should_use_worker(&self) -> bool {
        !self.escalated_to_lead
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

    pub fn record_turn_outcome(&mut self, stop_reason: StopReason, is_repetition_break: bool) {
        let is_worker_turn = self.current_turn_in_prompt >= self.lead_turns && self.worker_model.is_some();
        if is_worker_turn && (stop_reason == StopReason::Error || is_repetition_break) {
            self.escalated_to_lead = true;
        }
        self.current_turn_in_prompt += 1;
    }

    pub fn reset_for_new_prompt(&mut self) {
        self.current_turn_in_prompt = 0;
        self.escalated_to_lead = false;
    }
}
