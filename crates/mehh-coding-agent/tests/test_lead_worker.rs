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

#[test]
fn test_lead_worker_rescue_and_return_to_worker() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 1);
    mgr.record_turn_outcome(StopReason::Stop, false); // turn 0: lead
    assert_eq!(mgr.active_model_for_turn(1), "worker-model");

    // Worker errors -> Lead rescues for exactly 2 turns, then Worker resumes.
    mgr.record_turn_outcome(StopReason::Error, false); // turn 1: worker fails
    assert_eq!(mgr.active_model_for_turn(2), "lead-model");
    mgr.record_turn_outcome(StopReason::Stop, false); // rescue turn 1
    assert_eq!(mgr.active_model_for_turn(3), "lead-model");
    mgr.record_turn_outcome(StopReason::Stop, false); // rescue turn 2
    assert_eq!(mgr.active_model_for_turn(4), "worker-model");
}

#[test]
fn test_lead_worker_locks_after_third_escalation() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    for cycle in 0..3 {
        // Worker turn fails.
        mgr.record_turn_outcome(StopReason::Error, false);
        // Two Lead rescue turns.
        mgr.record_turn_outcome(StopReason::Stop, false);
        mgr.record_turn_outcome(StopReason::Stop, false);
        if cycle < 2 {
            assert_eq!(
                mgr.active_model_for_turn(mgr.current_turn_in_prompt),
                "worker-model",
                "worker must resume after rescue {cycle}"
            );
        }
    }
    // Third escalation locks to Lead for the rest of the prompt.
    for _ in 0..5 {
        assert_eq!(
            mgr.active_model_for_turn(mgr.current_turn_in_prompt),
            "lead-model"
        );
        mgr.record_turn_outcome(StopReason::Stop, false);
    }
}

#[test]
fn test_lead_worker_escalates_on_consecutive_tool_errors() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    assert_eq!(mgr.active_model_for_turn(0), "worker-model");
    mgr.record_tool_outcome(true);
    mgr.record_tool_outcome(true);
    // Turn itself ended fine, but two consecutive tool failures trigger rescue.
    mgr.record_turn_outcome(StopReason::Stop, false);
    assert_eq!(mgr.active_model_for_turn(1), "lead-model");
}

#[test]
fn test_lead_worker_reset_clears_rescue_and_lock() {
    let mut mgr = LeadWorkerManager::new_for_test("lead-model", "worker-model", 0);
    mgr.record_turn_outcome(StopReason::Error, false);
    assert_eq!(mgr.active_model_for_turn(1), "lead-model");
    mgr.reset_for_new_prompt();
    assert_eq!(mgr.active_model_for_turn(0), "worker-model");
}
