use mehh_ai::{faux_model, FauxProvider, LlmProvider, Model, StopReason};
use std::sync::Arc;

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

        Self::new(lead, faux_lead, Some(worker), Some(faux_worker), lead_turns)
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
        if !self.locked_to_lead && self.lead_rescue_remaining == 0 && turn >= self.lead_turns {
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
            self.worker_api_key.clone().or_else(|| {
                self.worker_model
                    .as_ref()
                    .filter(|w| w.provider.eq_ignore_ascii_case(&self.lead_model.provider))
                    .and(self.lead_api_key.as_ref())
                    .cloned()
            })
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
            self.consecutive_tool_errors = 0;
        } else {
            if self.lead_rescue_remaining > 0 {
                self.lead_rescue_remaining -= 1;
            }
            if !was_worker_turn {
                self.consecutive_tool_errors = 0;
            }
        }
        self.current_turn_in_prompt += 1;
    }

    /// Resolves the effective context window for an assistant response produced by `(model_id, provider)`.
    pub fn context_window_for(&self, model_id: &str, provider: &str) -> u64 {
        if let Some(worker) = self
            .worker_model
            .as_ref()
            .filter(|w| w.id == model_id && w.provider == provider)
        {
            worker.context_window
        } else if self.lead_model.id == model_id && self.lead_model.provider == provider {
            self.lead_model.context_window
        } else if let Some(worker) = self.worker_model.as_ref().filter(|w| w.id == model_id) {
            worker.context_window
        } else {
            self.lead_model.context_window
        }
    }

    pub fn reset_for_new_prompt(&mut self) {
        self.current_turn_in_prompt = 0;
        self.lead_rescue_remaining = 0;
        self.escalation_count = 0;
        self.locked_to_lead = false;
        self.consecutive_tool_errors = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_provider_worker_falls_back_to_lead_api_key_only_when_provider_matches() {
        let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
        mgr.lead_model.provider = "anthropic".to_string();
        mgr.worker_model.as_mut().unwrap().provider = "anthropic".to_string();
        mgr.set_lead_api_key(Some("sk-ant-lead".to_string()));
        mgr.set_worker_api_key(None);

        assert!(mgr.should_use_worker());
        assert_eq!(mgr.active_api_key().as_deref(), Some("sk-ant-lead"));

        // Different provider must never fall back to the lead key.
        mgr.worker_model.as_mut().unwrap().provider = "openai".to_string();
        assert_eq!(mgr.active_api_key(), None);
    }

    #[test]
    fn consecutive_tool_errors_persists_across_worker_turns_and_resets_on_lead_or_escalation() {
        let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
        assert!(mgr.should_use_worker());

        // Worker turn 0 has 1 tool error -> does not escalate yet, must retain count = 1.
        mgr.record_tool_outcome(true);
        mgr.record_turn_outcome(StopReason::ToolUse, false);
        assert!(mgr.should_use_worker(), "should stay on worker after 1 error");
        assert_eq!(mgr.consecutive_tool_errors, 1);

        // Worker turn 1 has a 2nd consecutive tool error -> reaches 2 -> escalates and resets.
        mgr.record_tool_outcome(true);
        mgr.record_turn_outcome(StopReason::ToolUse, false);
        assert!(!mgr.should_use_worker(), "should escalate to lead on 2nd error");
        assert_eq!(mgr.consecutive_tool_errors, 0);

        // Lead rescue turn with a tool error resets consecutive_tool_errors at end of Lead turn.
        mgr.record_tool_outcome(true);
        mgr.record_turn_outcome(StopReason::Stop, false);
        assert_eq!(mgr.consecutive_tool_errors, 0);

        // Successful tool outcome also resets.
        mgr.record_turn_outcome(StopReason::Stop, false); // finish rescue turn 2 -> back to worker
        assert!(mgr.should_use_worker());
        mgr.record_tool_outcome(true);
        assert_eq!(mgr.consecutive_tool_errors, 1);
        mgr.record_tool_outcome(false);
        assert_eq!(mgr.consecutive_tool_errors, 0);
    }

    #[test]
    fn context_window_for_matches_worker_and_lead_with_fallback() {
        let mut mgr = LeadWorkerManager::new_for_test("shared-id", "shared-id", 1);
        mgr.lead_model.provider = "anthropic".to_string();
        mgr.lead_model.context_window = 200_000;
        let worker = mgr.worker_model.as_mut().unwrap();
        worker.provider = "openai".to_string();
        worker.context_window = 16_000;

        assert_eq!(mgr.context_window_for("shared-id", "openai"), 16_000);
        assert_eq!(mgr.context_window_for("shared-id", "anthropic"), 200_000);

        mgr.worker_model.as_mut().unwrap().id = "worker-only".to_string();
        assert_eq!(mgr.context_window_for("worker-only", "other"), 16_000);
        assert_eq!(mgr.context_window_for("unknown", "other"), 200_000);
    }
}
