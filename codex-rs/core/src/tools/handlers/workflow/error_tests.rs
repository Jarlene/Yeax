//! Tests for workflow tool error contracts.

use super::*;
use crate::workflow::paths::WorkflowPathError;

fn every_error() -> Vec<WorkflowToolError> {
    vec![
        WorkflowToolError::Disabled,
        WorkflowToolError::InvalidInput("bad field".into()),
        WorkflowToolError::NotFound {
            name: "missing".into(),
            available: vec!["hello-workflow".into()],
        },
        WorkflowToolError::ResolveFailed("bad scope".into()),
        WorkflowToolError::ValidationFailed("META FAIL: x".into()),
        WorkflowToolError::ResumeUnknownRun {
            run_id: "wf_1".into(),
            resumable: vec!["wf_2".into()],
        },
        WorkflowToolError::LaunchFailed("boom".into()),
        WorkflowToolError::ControlUnknownRun {
            run_id: "wf_1".into(),
        },
        WorkflowToolError::ControlNotApplicable {
            run_id: "wf_1".into(),
            status: RunStatus::UserPaused,
            allowed: vec!["resume", "stop"],
        },
        WorkflowToolError::MaxActiveRunsExceeded { active: 5, max: 4 },
        WorkflowToolError::DepthExceeded,
        WorkflowToolError::NotAvailable("no manager".into()),
        WorkflowToolError::ScriptSymlinkRejected {
            path: "a/b.rhai".into(),
        },
    ]
}

#[test]
fn all_thirteen_codes_are_stable_and_unique() {
    let expected = [
        "workflows_disabled",
        "workflow_invalid_input",
        "workflow_not_found",
        "workflow_resolve_failed",
        "workflow_validation_failed",
        "workflow_resume_unknown_run",
        "workflow_launch_failed",
        "workflow_control_unknown_run",
        "workflow_control_not_applicable",
        "workflow_max_active_runs_exceeded",
        "workflow_depth_exceeded",
        "workflow_not_available",
        "workflow_script_symlink_rejected",
    ];
    let actual: Vec<_> = every_error().iter().map(WorkflowToolError::code).collect();
    assert_eq!(actual, expected);
    let mut unique = actual.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 13);
}

#[test]
fn user_visible_messages_carry_actionable_detail() {
    for error in every_error() {
        let message = error.user_visible();
        assert!(!message.is_empty(), "{}", error.code());
    }
    assert!(
        WorkflowToolError::NotFound {
            name: "missing".into(),
            available: vec!["a".into(), "b".into()],
        }
        .user_visible()
        .contains("a, b")
    );
    assert!(
        WorkflowToolError::ResumeUnknownRun {
            run_id: "wf_1".into(),
            resumable: vec!["wf_2".into()],
        }
        .user_visible()
        .contains("wf_2")
    );
    assert!(
        WorkflowToolError::ControlNotApplicable {
            run_id: "wf_1".into(),
            status: RunStatus::UserPaused,
            allowed: vec!["resume"],
        }
        .user_visible()
        .contains("resume")
    );
    assert!(
        WorkflowToolError::MaxActiveRunsExceeded { active: 5, max: 4 }
            .user_visible()
            .contains('4')
    );
}

#[test]
fn control_errors_map_per_endpoint() {
    let paths = WorkflowControlError::Paths(WorkflowPathError::InvalidRunId("..".into()));
    assert_eq!(
        from_control(&paths, WorkflowControlEndpoint::Resume).code(),
        "workflow_not_available"
    );
    let unavailable = WorkflowControlError::Unavailable("x".into());
    assert_eq!(
        from_control(&unavailable, WorkflowControlEndpoint::Stop).code(),
        "workflow_not_available"
    );
    let internal = WorkflowControlError::Internal("x".into());
    assert_eq!(
        from_control(&internal, WorkflowControlEndpoint::Pause).code(),
        "workflow_not_available"
    );

    let missing = WorkflowControlError::RunNotFound("wf_9".into());
    assert_eq!(
        from_control(&missing, WorkflowControlEndpoint::Resume).code(),
        "workflow_resume_unknown_run"
    );
    assert_eq!(
        from_control(&missing, WorkflowControlEndpoint::Pause).code(),
        "workflow_control_unknown_run"
    );
    assert_eq!(
        from_control(&missing, WorkflowControlEndpoint::Stop).code(),
        "workflow_control_unknown_run"
    );

    let not_applicable = WorkflowControlError::NotApplicable {
        run_id: "wf_1".into(),
        status: RunStatus::Complete,
        allowed: vec!["resume"],
    };
    assert_eq!(
        from_control(&not_applicable, WorkflowControlEndpoint::Stop).code(),
        "workflow_control_not_applicable"
    );

    let exceeded = WorkflowControlError::MaxActiveRunsExceeded { active: 5, max: 4 };
    assert_eq!(
        from_control(&exceeded, WorkflowControlEndpoint::Pause).code(),
        "workflow_max_active_runs_exceeded"
    );
}
