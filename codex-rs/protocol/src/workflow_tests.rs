//! Tests for the workflow protocol types.

use super::*;

use pretty_assertions::assert_eq;
use serde_json::json;

fn sample_event() -> WorkflowUpdatedEvent {
    WorkflowUpdatedEvent {
        thread_id: ThreadId::from_u128(7),
        run_id: "run-1".to_string(),
        revision: 3,
        name: "hello-workflow".to_string(),
        objective: "verify migration".to_string(),
        status: "active".to_string(),
        foreground: true,
        phases: vec![WorkflowPhaseInfo {
            title: "research".to_string(),
            state: "running".to_string(),
        }],
        current_phase: Some("research".to_string()),
        agent_budget: Some(4),
        agents_used: 1,
        agents_reserved: 1,
        agents_remaining: 2,
        agent_usage_incomplete: true,
        elapsed_ms: 1_500,
        active_agents: 1,
        current_agent_label: Some("researcher".to_string()),
        agents: vec![WorkflowAgentInfo {
            agent_id: "agent-1".to_string(),
            label: "researcher".to_string(),
            phase: Some("research".to_string()),
            model: Some("gpt-5".to_string()),
            state: "active".to_string(),
            tokens_used: 0,
            duration_ms: 1_500,
        }],
        last_event: Some("phase_started".to_string()),
        last_event_detail: Some("research".to_string()),
        last_event_timestamp: Some("2026-11-23T00:00:00Z".to_string()),
        pause_message: Some("paused by user".to_string()),
        result_summary: Some("done".to_string()),
        degraded_fields: vec!["tokens_used".to_string()],
    }
}

#[test]
fn workflow_updated_event_keeps_route_keys_and_minimum_fields() {
    let event = sample_event();
    let value = serde_json::to_value(&event).expect("serialize");
    assert_eq!(
        value["threadId"],
        json!("00000000-0000-0000-0000-000000000007")
    );
    assert_eq!(value["runId"], json!("run-1"));
    assert_eq!(value["revision"], json!(3));
    for key in [
        "threadId",
        "runId",
        "revision",
        "name",
        "objective",
        "status",
        "foreground",
        "phases",
        "currentPhase",
        "agentBudget",
        "agentsUsed",
        "agentsReserved",
        "agentsRemaining",
        "agentUsageIncomplete",
        "elapsedMs",
        "activeAgents",
        "currentAgentLabel",
        "agents",
        "lastEvent",
        "lastEventDetail",
        "lastEventTimestamp",
        "pauseMessage",
        "resultSummary",
        "degradedFields",
    ] {
        assert!(value.get(key).is_some(), "missing field {key}");
    }
}

#[test]
fn workflow_updated_event_covers_eleven_statuses() {
    assert_eq!(WORKFLOW_STATUSES.len(), 11);
    for status in WORKFLOW_STATUSES {
        let mut event = sample_event();
        event.status = status.to_string();
        assert!(event.is_known_status(), "status {status} not recognized");
    }

    let mut event = sample_event();
    event.status = "not_a_status".to_string();
    assert!(!event.is_known_status());
    let value = serde_json::to_value(&event).expect("serialize unknown status");
    assert_eq!(value["status"], json!("not_a_status"));
}

#[test]
fn workflow_updated_event_revision_is_strictly_monotonic() {
    let mut event = sample_event();
    event.revision = 5;
    assert!(event.should_apply(None));
    assert!(event.should_apply(Some(4)));
    assert!(!event.should_apply(Some(5)));
    assert!(!event.should_apply(Some(6)));
}

#[test]
fn workflow_updated_event_terminal_statuses_are_complete_failed_cancelled() {
    let mut event = sample_event();
    for terminal in ["complete", "failed", "cancelled"] {
        event.status = terminal.to_string();
        assert!(event.is_terminal(), "{terminal} must be terminal");
    }
    for non_terminal in WORKFLOW_STATUSES
        .iter()
        .filter(|status| !["complete", "failed", "cancelled"].contains(status))
    {
        event.status = (*non_terminal).to_string();
        assert!(!event.is_terminal(), "{non_terminal} must not be terminal");
    }
}

#[test]
fn workflow_updated_event_serializes_through_event_msg() {
    let message = crate::protocol::EventMsg::WorkflowUpdated(sample_event());
    let value = serde_json::to_value(&message).expect("serialize event");
    assert_eq!(value["type"], json!("workflow_updated"), "got {value}");
    let restored: crate::protocol::EventMsg =
        serde_json::from_value(value.clone()).expect("deserialize event");
    let crate::protocol::EventMsg::WorkflowUpdated(restored) = restored else {
        panic!("expected WorkflowUpdated, got {restored:?}");
    };
    assert_eq!(restored, sample_event());
}

#[test]
fn workflow_updated_event_never_carries_scripts_prompts_or_tokens() {
    let value = serde_json::to_value(sample_event()).expect("serialize");
    let object = value.as_object().expect("object");
    for key in object.keys() {
        let lowered = key.to_ascii_lowercase();
        assert!(!lowered.contains("script"), "unexpected key {key}");
        assert!(!lowered.contains("prompt"), "unexpected key {key}");
        assert!(!lowered.contains("token_"), "unexpected key {key}");
        assert!(!lowered.contains("credential"), "unexpected key {key}");
    }
}

#[test]
fn workflow_updated_event_tolerates_unknown_fields_and_missing_optional_ones() {
    let value = json!({
        "threadId": "00000000-0000-0000-0000-000000000007",
        "runId": "run-1",
        "revision": 1,
        "name": "hello-workflow",
        "objective": "",
        "status": "complete",
        "foreground": false,
        "agentsUsed": 0,
        "agentsReserved": 0,
        "agentsRemaining": 0,
        "agentUsageIncomplete": true,
        "elapsedMs": 0,
        "activeAgents": 0,
        "futureField": { "ignored": true }
    });
    let event: WorkflowUpdatedEvent = serde_json::from_value(value).expect("deserialize");
    assert_eq!(event.status, "complete");
    assert!(event.is_terminal());
    assert!(event.phases.is_empty());
    assert!(event.agents.is_empty());
    assert!(event.degraded_fields.is_empty());
    assert_eq!(event.agent_budget, None);
}
