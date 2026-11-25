//! Tests for workflow notifications.

use super::*;

use pretty_assertions::assert_eq;

use super::super::tracker::AgentRow;
use super::super::tracker::HistoryRow;
use super::super::tracker::RunStatus;

fn snapshot(status: RunStatus, revision: u64) -> WorkflowSnapshot {
    WorkflowSnapshot {
        run_id: "run-1".to_string(),
        name: "hello-workflow".to_string(),
        objective: "verify".to_string(),
        status,
        foreground: true,
        revision,
        phases: vec!["research".to_string(), "report".to_string()],
        current_phase: Some("research".to_string()),
        budget: super::super::tracker::BudgetView {
            total: Some(4),
            spent: 1,
            reserved: 1,
            remaining: Some(2),
            usage_incomplete: true,
        },
        elapsed_ms: 250,
        active_agents: 1,
        current_agent_label: Some("researcher".to_string()),
        agents: vec![AgentRow {
            agent_id: "agent-1".to_string(),
            label: "researcher".to_string(),
            phase: AgentPhase::Running,
            workflow_phase: Some("research".to_string()),
            started_at_ms: 1_000,
            finished_at_ms: None,
            tokens_used: 0,
        }],
        history: vec![HistoryRow {
            at_ms: 1_000,
            kind: "phase".to_string(),
            message: "research".to_string(),
        }],
        last_event: Some("phase_started".to_string()),
        last_event_detail: Some("research".to_string()),
        last_event_at_ms: Some(1_000),
        pause_message: None,
        result_summary: None,
        degraded_fields: Vec::new(),
    }
}

#[test]
fn snapshot_maps_every_minimum_field() {
    let event = snapshot_to_event(
        ThreadId::from_u128(1),
        &snapshot(RunStatus::Active, 7),
        1_250,
    );
    assert_eq!(event.run_id, "run-1");
    assert_eq!(event.revision, 7);
    assert_eq!(event.status, "active");
    assert_eq!(event.agent_budget, Some(4));
    assert_eq!(event.agents_used, 1);
    assert_eq!(event.agents_reserved, 1);
    assert_eq!(event.agents_remaining, 2);
    assert!(event.agent_usage_incomplete);
    assert_eq!(event.elapsed_ms, 250);
    assert_eq!(event.active_agents, 1);
    assert_eq!(event.current_phase.as_deref(), Some("research"));
    assert_eq!(event.phases.len(), 2);
    assert_eq!(event.phases[0].state, "active");
    assert_eq!(event.phases[1].state, "pending");
    assert_eq!(event.agents.len(), 1);
    assert_eq!(event.agents[0].duration_ms, 250);
    assert!(event.last_event_timestamp.is_some());
    assert_eq!(event.degraded_fields, vec!["agents.model".to_string()]);
}

#[test]
fn snapshot_reports_all_eleven_statuses() {
    for status in RunStatus::ALL {
        let event = snapshot_to_event(ThreadId::from_u128(1), &snapshot(status, 1), 1_000);
        assert!(is_known_status(status));
        assert_eq!(event.status, status.as_str());
    }
}

#[test]
fn terminal_snapshot_is_forwarded_only_once() {
    let mut gate = RevisionGate::default();
    let terminal = snapshot(RunStatus::Complete, 9);
    assert!(gate.accept(&terminal));
    assert!(!gate.accept(&terminal));

    let mut newer_terminal = terminal.clone();
    newer_terminal.revision = 10;
    assert!(!gate.accept(&newer_terminal), "terminal must not repeat");

    let other_run = snapshot(RunStatus::Complete, 1);
    let mut other_run = other_run;
    other_run.run_id = "run-2".to_string();
    assert!(gate.accept(&other_run), "gate is per run");
}

#[test]
fn stale_revision_is_rejected() {
    let mut gate = RevisionGate::default();
    assert!(gate.accept(&snapshot(RunStatus::Active, 5)));
    assert!(!gate.accept(&snapshot(RunStatus::Active, 5)));
    assert!(!gate.accept(&snapshot(RunStatus::Active, 4)));
    assert!(gate.accept(&snapshot(RunStatus::Active, 6)));
}

#[test]
fn event_never_contains_script_or_prompt_text() {
    let event = snapshot_to_event(
        ThreadId::from_u128(1),
        &snapshot(RunStatus::Active, 1),
        1_250,
    );
    let value = serde_json::to_value(&event).expect("serialize");
    let object = value.as_object().expect("object");
    for key in object.keys() {
        let lowered = key.to_ascii_lowercase();
        assert!(!lowered.contains("script"));
        assert!(!lowered.contains("prompt"));
        assert!(!lowered.contains("credential"));
    }
}
