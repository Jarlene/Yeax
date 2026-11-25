//! Tests for the v2 workflow protocol types.

use super::*;

use pretty_assertions::assert_eq;
use serde_json::json;

fn summary() -> WorkflowRunSummary {
    WorkflowRunSummary {
        run_id: "run-1".to_string(),
        name: "hello-workflow".to_string(),
        status: "active".to_string(),
        phase: Some("research".to_string()),
        started_at_ms: 1,
        updated_at_ms: 2,
        agent_count: 3,
        script_path: "/tmp/run/script.rhai".to_string(),
        run_dir: "/tmp/run".to_string(),
        degraded_fields: vec!["model".to_string()],
    }
}

#[test]
fn workflow_run_summary_round_trips_with_camel_case_fields() {
    let value = serde_json::to_value(summary()).expect("serialize");
    assert_eq!(value["runId"], json!("run-1"));
    assert_eq!(value["startedAtMs"], json!(1));
    assert_eq!(value["updatedAtMs"], json!(2));
    assert_eq!(value["agentCount"], json!(3));
    assert_eq!(value["scriptPath"], json!("/tmp/run/script.rhai"));
    assert_eq!(value["runDir"], json!("/tmp/run"));
    assert_eq!(value["degradedFields"], json!(["model"]));
    let restored: WorkflowRunSummary = serde_json::from_value(value).expect("deserialize");
    assert_eq!(restored, summary());
}

#[test]
fn workflow_control_requests_all_carry_thread_id() {
    let cases = [
        serde_json::to_value(WorkflowListParams {
            thread_id: "thread-1".to_string(),
            include_completed: false,
        })
        .expect("list"),
        serde_json::to_value(WorkflowPauseParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
        })
        .expect("pause"),
        serde_json::to_value(WorkflowResumeParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
        })
        .expect("resume"),
        serde_json::to_value(WorkflowStopParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
        })
        .expect("stop"),
        serde_json::to_value(WorkflowSaveParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            name: "saved".to_string(),
        })
        .expect("save"),
    ];
    for value in cases {
        assert_eq!(value["threadId"], json!("thread-1"), "got {value}");
    }
}

#[test]
fn workflow_responses_round_trip() {
    let list = WorkflowListResponse {
        runs: vec![summary()],
    };
    let value = serde_json::to_value(&list).expect("serialize list");
    let restored: WorkflowListResponse = serde_json::from_value(value).expect("deserialize");
    assert_eq!(restored, list);

    let control = WorkflowControlResponse { run: summary() };
    let value = serde_json::to_value(&control).expect("serialize control");
    let restored: WorkflowControlResponse = serde_json::from_value(value).expect("deserialize");
    assert_eq!(restored, control);

    let save = WorkflowSaveResponse {
        path: "/tmp/workflows/saved.rhai".to_string(),
    };
    let value = serde_json::to_value(&save).expect("serialize save");
    let restored: WorkflowSaveResponse = serde_json::from_value(value).expect("deserialize");
    assert_eq!(restored, save);
}

#[test]
fn workflow_error_codes_are_the_nine_registered_stable_codes() {
    let expected = [
        "workflows_disabled",
        "workflow_invalid_input",
        "workflow_resume_unknown_run",
        "workflow_control_unknown_run",
        "workflow_control_not_applicable",
        "workflow_max_active_runs_exceeded",
        "workflow_not_available",
        "workflow_save_exists",
        "workflow_save_failed",
    ];
    let codes = [
        WorkflowErrorCode::WorkflowsDisabled,
        WorkflowErrorCode::WorkflowInvalidInput,
        WorkflowErrorCode::WorkflowResumeUnknownRun,
        WorkflowErrorCode::WorkflowControlUnknownRun,
        WorkflowErrorCode::WorkflowControlNotApplicable,
        WorkflowErrorCode::WorkflowMaxActiveRunsExceeded,
        WorkflowErrorCode::WorkflowNotAvailable,
        WorkflowErrorCode::WorkflowSaveExists,
        WorkflowErrorCode::WorkflowSaveFailed,
    ];
    assert_eq!(codes.len(), expected.len());
    for (code, wire) in codes.iter().zip(expected) {
        assert_eq!(code.as_str(), wire);
        assert_eq!(serde_json::to_value(code).expect("serialize"), json!(wire));
    }
}

#[test]
fn workflow_error_data_serializes_code_snake_case_and_optional_detail() {
    let data = WorkflowErrorData {
        code: WorkflowErrorCode::WorkflowSaveFailed,
        detail: Some("EACCES: permission denied".to_string()),
    };
    let value = serde_json::to_value(&data).expect("serialize");
    assert_eq!(value["code"], json!("workflow_save_failed"));
    assert_eq!(value["detail"], json!("EACCES: permission denied"));

    let without_detail = WorkflowErrorData {
        code: WorkflowErrorCode::WorkflowNotAvailable,
        detail: None,
    };
    let value = serde_json::to_value(&without_detail).expect("serialize");
    assert_eq!(value["code"], json!("workflow_not_available"));
    assert!(value.get("detail").is_none());
}

#[test]
fn removed_error_codes_are_not_part_of_the_contract() {
    let value = serde_json::to_string(&WorkflowErrorData {
        code: WorkflowErrorCode::WorkflowInvalidInput,
        detail: None,
    })
    .expect("serialize");
    for removed in [
        "workflow_run_not_found",
        "workflow_thread_not_found",
        "workflow_name_invalid",
        "workflow_internal_error",
        "workflow_launch_failed",
    ] {
        assert!(!value.contains(removed), "removed code {removed} leaked");
    }
}

#[test]
fn workflow_updated_notification_round_trips() {
    let notification = WorkflowUpdatedNotification {
        thread_id: "thread-1".to_string(),
        run_id: "run-1".to_string(),
        revision: 4,
        name: "hello-workflow".to_string(),
        objective: "test".to_string(),
        status: "complete".to_string(),
        foreground: false,
        phases: vec![WorkflowPhaseInfo {
            title: "research".to_string(),
            state: "done".to_string(),
        }],
        current_phase: None,
        agent_budget: Some(4),
        agents_used: 1,
        agents_reserved: 0,
        agents_remaining: 3,
        agent_usage_incomplete: false,
        elapsed_ms: 10,
        active_agents: 0,
        current_agent_label: None,
        agents: vec![WorkflowAgentInfo {
            agent_id: "agent-1".to_string(),
            label: "researcher".to_string(),
            phase: None,
            model: None,
            state: "completed".to_string(),
            tokens_used: 12,
            duration_ms: 9,
        }],
        last_event: Some("finished".to_string()),
        last_event_detail: None,
        last_event_timestamp: None,
        pause_message: None,
        result_summary: Some("ok".to_string()),
        degraded_fields: vec!["model".to_string()],
    };
    let value = serde_json::to_value(&notification).expect("serialize");
    assert_eq!(value["threadId"], json!("thread-1"));
    assert_eq!(value["revision"], json!(4));
    let restored: WorkflowUpdatedNotification = serde_json::from_value(value).expect("deserialize");
    assert_eq!(restored, notification);
}
