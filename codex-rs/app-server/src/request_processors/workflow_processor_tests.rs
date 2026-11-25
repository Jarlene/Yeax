//! Tests for the app-server workflow request processor.

use super::*;

use crate::error_code::INTERNAL_ERROR_CODE;
use crate::error_code::INVALID_PARAMS_ERROR_CODE;
use crate::error_code::METHOD_NOT_FOUND_ERROR_CODE;
use codex_app_server_protocol::JSONRPCErrorError;
use pretty_assertions::assert_eq;

fn error_data(error: &JSONRPCErrorError) -> WorkflowErrorData {
    serde_json::from_value(error.data.clone().expect("error data")).expect("workflow error data")
}

#[test]
fn control_endpoint_unknown_run_maps_per_endpoint() {
    let error = WorkflowControlError::RunNotFound("run-1".to_string());

    let mapped = map_control_error(&error, WorkflowEndpoint::Resume);
    assert_eq!(mapped.code, INVALID_PARAMS_ERROR_CODE);
    assert_eq!(mapped.message, "workflow unknown run");
    assert_eq!(
        error_data(&mapped).code,
        WorkflowErrorCode::WorkflowResumeUnknownRun
    );

    for endpoint in [WorkflowEndpoint::Pause, WorkflowEndpoint::Stop] {
        let mapped = map_control_error(&error, endpoint);
        assert_eq!(
            error_data(&mapped).code,
            WorkflowErrorCode::WorkflowControlUnknownRun
        );
    }

    let mapped = map_control_error(&error, WorkflowEndpoint::Save);
    assert_eq!(
        error_data(&mapped).code,
        WorkflowErrorCode::WorkflowInvalidInput
    );
    assert_eq!(error_data(&mapped).detail.as_deref(), Some("run-1"));
}

#[test]
fn save_internal_failure_maps_to_save_failed_with_errno_detail() {
    let error = WorkflowControlError::Internal("EACCES: permission denied".to_string());
    let mapped = map_control_error(&error, WorkflowEndpoint::Save);
    assert_eq!(mapped.code, INTERNAL_ERROR_CODE);
    assert_eq!(mapped.message, "workflow save failed");
    let data = error_data(&mapped);
    assert_eq!(data.code, WorkflowErrorCode::WorkflowSaveFailed);
    assert_eq!(data.detail.as_deref(), Some("EACCES: permission denied"));
}

#[test]
fn control_internal_failure_maps_to_not_available() {
    let error = WorkflowControlError::Internal("boom".to_string());
    for endpoint in [
        WorkflowEndpoint::List,
        WorkflowEndpoint::Pause,
        WorkflowEndpoint::Stop,
    ] {
        let mapped = map_control_error(&error, endpoint);
        assert_eq!(mapped.code, INTERNAL_ERROR_CODE);
        assert_eq!(mapped.message, "workflow not available");
        assert_eq!(
            error_data(&mapped).code,
            WorkflowErrorCode::WorkflowNotAvailable
        );
    }
}

#[test]
fn unavailable_and_launch_failure_map_to_not_available() {
    for error in [
        WorkflowControlError::Unavailable("no manager".to_string()),
        WorkflowControlError::LaunchFailed("publish failed".to_string()),
    ] {
        let mapped = map_control_error(&error, WorkflowEndpoint::List);
        assert_eq!(mapped.code, INTERNAL_ERROR_CODE);
        assert_eq!(
            error_data(&mapped).code,
            WorkflowErrorCode::WorkflowNotAvailable
        );
    }
}

#[test]
fn disabled_maps_to_method_not_found_with_workflows_disabled_code() {
    let mapped = map_control_error(&WorkflowControlError::Disabled, WorkflowEndpoint::List);
    assert_eq!(mapped.code, METHOD_NOT_FOUND_ERROR_CODE);
    assert!(mapped.message.contains("[features] workflows = true"));
    assert_eq!(
        error_data(&mapped).code,
        WorkflowErrorCode::WorkflowsDisabled
    );
}

#[test]
fn capacity_and_save_state_errors_keep_their_codes() {
    let mapped = map_control_error(
        &WorkflowControlError::MaxActiveRunsExceeded { active: 5, max: 4 },
        WorkflowEndpoint::Pause,
    );
    assert_eq!(mapped.code, INVALID_PARAMS_ERROR_CODE);
    assert_eq!(
        error_data(&mapped).code,
        WorkflowErrorCode::WorkflowMaxActiveRunsExceeded
    );

    let mapped = map_control_error(
        &WorkflowControlError::InvalidName("bad/name".to_string()),
        WorkflowEndpoint::Save,
    );
    assert_eq!(mapped.message, "workflow name invalid");
    assert_eq!(
        error_data(&mapped).code,
        WorkflowErrorCode::WorkflowInvalidInput
    );

    let mapped = map_control_error(
        &WorkflowControlError::SaveExists(std::path::PathBuf::from("/tmp/x.rhai")),
        WorkflowEndpoint::Save,
    );
    assert_eq!(
        error_data(&mapped).code,
        WorkflowErrorCode::WorkflowSaveExists
    );
}

#[test]
fn dto_mapping_preserves_run_summary_fields() {
    let summary = CoreRunSummary {
        run_id: "run-1".to_string(),
        name: "hello-workflow".to_string(),
        status: "active".to_string(),
        phase: Some("research".to_string()),
        started_at_ms: 1,
        updated_at_ms: 2,
        agent_count: 3,
        script_path: std::path::PathBuf::from("/tmp/run/script.rhai"),
        run_dir: std::path::PathBuf::from("/tmp/run"),
        degraded_fields: vec!["agents.model".to_string()],
    };
    let dto = to_dto(summary);
    assert_eq!(dto.run_id, "run-1");
    assert_eq!(dto.script_path, "/tmp/run/script.rhai");
    assert_eq!(dto.run_dir, "/tmp/run");
    assert_eq!(dto.agent_count, 3);
    assert_eq!(dto.degraded_fields, vec!["agents.model".to_string()]);
}

#[test]
fn invalid_thread_id_is_reported_as_invalid_input() {
    let error = thread_not_found("not-a-uuid");
    assert_eq!(error.code, INVALID_PARAMS_ERROR_CODE);
    assert_eq!(error.message, "workflow thread not found");
    let data = error_data(&error);
    assert_eq!(data.code, WorkflowErrorCode::WorkflowInvalidInput);
    assert_eq!(data.detail.as_deref(), Some("not-a-uuid"));
}

#[test]
fn removed_error_codes_never_appear() {
    for error in [
        WorkflowControlError::Unavailable("x".to_string()),
        WorkflowControlError::Internal("x".to_string()),
        WorkflowControlError::Disabled,
        WorkflowControlError::LaunchFailed("x".to_string()),
        WorkflowControlError::RunNotFound("x".to_string()),
        WorkflowControlError::MaxActiveRunsExceeded { active: 5, max: 4 },
        WorkflowControlError::InvalidName("x".to_string()),
        WorkflowControlError::SaveExists(std::path::PathBuf::from("/tmp/x")),
        WorkflowControlError::SaveFailed {
            path: std::path::PathBuf::from("/tmp/x"),
            detail: "EIO".to_string(),
        },
    ] {
        let mapped = map_control_error(&error, WorkflowEndpoint::Save);
        let serialized = serde_json::to_string(&mapped).expect("serialize");
        for removed in [
            "workflow_run_not_found",
            "workflow_thread_not_found",
            "workflow_name_invalid",
            "workflow_internal_error",
            "workflow_launch_failed",
        ] {
            assert!(!serialized.contains(removed), "leaked {removed}");
        }
    }
}
