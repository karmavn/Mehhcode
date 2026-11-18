use mehh_coding_agent::lead_worker::LeadWorkerManager;
use mehh_ai::StopReason;

#[test]
fn test_lead_worker_escalates_on_provider_error() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 1);

    // Turn 0: Lead completes successfully
    assert_eq!(mgr.active_model_for_turn(0), "lead-model");
    mgr.record_turn_outcome(StopReason::Stop, false);

    // Turn 1: Switches to Worker
    assert_eq!(mgr.active_model_for_turn(1), "worker-model");

    // Worker hits StopReason::Error -> escalates back to Lead
    mgr.record_turn_outcome(StopReason::Error, false);
    assert_eq!(mgr.active_model_for_turn(2), "lead-model");
}

#[test]
fn test_lead_worker_resets_on_new_prompt() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 2);

    assert_eq!(mgr.active_model_for_turn(0), "lead-model");
    mgr.record_turn_outcome(StopReason::Stop, false);
    assert_eq!(mgr.active_model_for_turn(1), "lead-model");
    mgr.record_turn_outcome(StopReason::Stop, false);
    assert_eq!(mgr.active_model_for_turn(2), "worker-model");

    // Reset for new user prompt resets turn count back to Lead
    mgr.reset_for_new_prompt();
    assert_eq!(mgr.active_model_for_turn(0), "lead-model");
}
