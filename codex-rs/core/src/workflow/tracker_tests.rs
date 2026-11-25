//! Tests for workflow state tracking.

use super::*;

fn tracker() -> Tracker {
    Tracker::new(
        "wf_1",
        "probe",
        "do it",
        vec!["Plan".into()],
        Some(4),
        true,
        0,
    )
}

#[test]
fn eleven_statuses_round_trip_snake_case() {
    assert_eq!(RunStatus::ALL.len(), 11);
    for status in RunStatus::ALL {
        let json = serde_json::to_string(&status).expect("serialize");
        assert_eq!(json, format!("\"{}\"", status.as_str()));
        assert_eq!(status.as_str(), status.as_str().to_ascii_lowercase());
    }
}

#[test]
fn pause_only_from_active() {
    let mut tracker = tracker();
    tracker.pause(PauseReason::User, "asked").expect("pause");
    assert_eq!(tracker.status(), RunStatus::UserPaused);
    let error = tracker
        .pause(PauseReason::User, "again")
        .expect_err("reject");
    assert!(matches!(error, TrackerError::InvalidTransition { .. }));
}

#[test]
fn every_pause_reason_maps_to_its_status() {
    for (reason, expected) in [
        (PauseReason::User, RunStatus::UserPaused),
        (PauseReason::BackOff, RunStatus::BackOffPaused),
        (PauseReason::NoProgress, RunStatus::NoProgressPaused),
        (PauseReason::Infra, RunStatus::InfraPaused),
        (PauseReason::Verification, RunStatus::Blocked),
    ] {
        let mut tracker = tracker();
        tracker.pause(reason, "paused").expect("pause");
        assert_eq!(tracker.status(), expected);
        tracker.resume(None).expect("resume");
        assert_eq!(tracker.status(), RunStatus::Active);
    }
}

#[test]
fn budget_limited_requires_a_higher_budget_to_resume() {
    let mut tracker = tracker();
    tracker.budget_limited(Some("out".into())).expect("limit");
    assert_eq!(tracker.status(), RunStatus::BudgetLimited);
    assert!(tracker.status().is_reportable_completion());
    assert!(!tracker.status().is_terminal());

    assert!(matches!(
        tracker.resume(None).expect_err("no budget"),
        TrackerError::ResumeRequiresHigherBudget { .. }
    ));
    assert!(matches!(
        tracker.resume(Some(4)).expect_err("not higher"),
        TrackerError::ResumeRequiresHigherBudget { .. }
    ));
    tracker.resume(Some(8)).expect("higher budget");
    assert_eq!(tracker.status(), RunStatus::Active);
    assert_eq!(tracker.snapshot().budget.total, Some(8));
}

#[test]
fn interrupted_cannot_resume() {
    let mut tracker = tracker();
    tracker.mark_interrupted().expect("interrupt");
    assert!(!RunStatus::Interrupted.can_resume());
    assert!(matches!(
        tracker.resume(Some(8)).expect_err("reject"),
        TrackerError::InvalidTransition { .. }
    ));
}

#[test]
fn terminal_state_is_reported_once_and_stop_is_rejected_after() {
    let mut tracker = tracker();
    tracker.complete(Some("done".into())).expect("complete");
    let revision = tracker.revision();
    tracker
        .complete(Some("done again".into()))
        .expect("idempotent");
    assert_eq!(tracker.revision(), revision);
    assert!(matches!(
        tracker.stop().expect_err("reject"),
        TrackerError::InvalidTransition { .. }
    ));
}

#[test]
fn failed_can_be_resumed_after_prune() {
    let mut tracker = tracker();
    tracker.fail(Some("boom".into())).expect("fail");
    assert!(tracker.status().can_resume());
    tracker.resume(None).expect("resume");
    assert_eq!(tracker.status(), RunStatus::Active);
}

#[test]
fn stop_only_from_non_terminal() {
    let mut tracker = tracker();
    tracker.stop().expect("stop");
    assert_eq!(tracker.status(), RunStatus::Cancelled);
}

#[test]
fn revision_is_monotonic() {
    let mut tracker = tracker();
    let mut last = tracker.revision();
    tracker.set_phase("Plan").expect("phase");
    assert!(tracker.revision() > last);
    last = tracker.revision();
    tracker.reserve_agents(1).expect("reserve");
    assert!(tracker.revision() > last);
    last = tracker.revision();
    tracker.agent_started("t1", "greeter", Some("Plan".into()), 10);
    assert!(tracker.revision() > last);
}

#[test]
fn phase_is_recorded_once_and_becomes_current() {
    let mut tracker = tracker();
    tracker.set_phase("Plan").expect("phase");
    tracker.set_phase("Plan").expect("phase again");
    assert_eq!(tracker.snapshot().phases, vec!["Plan"]);
    tracker.set_phase("Report").expect("phase");
    assert_eq!(tracker.snapshot().current_phase.as_deref(), Some("Report"));
}

#[test]
fn history_is_capped() {
    let mut tracker = tracker();
    for index in 0..MAX_HISTORY_ROWS + 20 {
        tracker
            .push_event("log", format!("line {index}"), None)
            .expect("log");
    }
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.history.len(), MAX_HISTORY_ROWS);
    assert_eq!(
        snapshot.history.last().map(|row| row.message.as_str()),
        Some("line 219")
    );
}

#[test]
fn oversized_event_message_is_rejected() {
    let mut tracker = tracker();
    let error = tracker
        .push_event("log", "x".repeat(MAX_EVENT_MESSAGE_BYTES + 1), None)
        .expect_err("too long");
    assert!(matches!(error, TrackerError::HistoryMessageTooLong { .. }));
}

#[test]
fn agent_rows_track_reservations_usage_and_caps() {
    let mut tracker = Tracker::new("wf_1", "probe", "do it", vec![], Some(100), true, 0);
    tracker.reserve_agents(2).expect("reserve");
    assert_eq!(tracker.snapshot().budget.remaining, Some(98));
    tracker.agent_started("t1", "one", None, 1);
    tracker.agent_finished("t1", AgentPhase::Completed, 7, 2);
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.budget.spent, 1);
    assert_eq!(snapshot.budget.reserved, 1);
    assert_eq!(snapshot.agents.len(), 1);
    assert_eq!(snapshot.agents[0].phase, AgentPhase::Completed);
    assert_eq!(snapshot.active_agents, 0);
    assert_eq!(snapshot.current_agent_label, None);
}

#[test]
fn finished_agent_rows_are_trimmed_before_running_ones() {
    let mut tracker = Tracker::new("wf_1", "probe", "do it", vec![], None, true, 0);
    for index in 0..(MAX_AGENT_ROWS + 5) {
        let id = format!("t{index}");
        tracker.agent_started(&id, id.clone(), None, index as u64);
        tracker.agent_finished(&id, AgentPhase::Completed, 0, index as u64);
    }
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.agents.len(), MAX_AGENT_ROWS);
    assert_eq!(
        snapshot.agents.last().map(|row| row.agent_id.as_str()),
        Some("t68")
    );
}

#[test]
fn degraded_fields_are_deduplicated() {
    let mut tracker = tracker();
    tracker.note_degraded("tokens_used");
    tracker.note_degraded("tokens_used");
    tracker.note_degraded("parent_turn_id");
    assert_eq!(
        tracker.snapshot().degraded_fields,
        vec!["tokens_used", "parent_turn_id"]
    );
}

#[test]
fn terminal_reports_include_result_summary() {
    let mut tracker = tracker();
    tracker.complete(Some("all good".into())).expect("complete");
    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.status, RunStatus::Complete);
    assert_eq!(snapshot.result_summary.as_deref(), Some("all good"));
}
