//! Single owner of workflow tool error codes. Callers map typed errors here;
//! they must not invent codes or duplicate capacity/state decisions.

use crate::workflow::manager::WorkflowControlError;
use crate::workflow::tracker::RunStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowToolError {
    Disabled,
    InvalidInput(String),
    NotFound {
        name: String,
        available: Vec<String>,
    },
    ResolveFailed(String),
    ValidationFailed(String),
    ResumeUnknownRun {
        run_id: String,
        resumable: Vec<String>,
    },
    LaunchFailed(String),
    ControlUnknownRun {
        run_id: String,
    },
    ControlNotApplicable {
        run_id: String,
        status: RunStatus,
        allowed: Vec<&'static str>,
    },
    MaxActiveRunsExceeded {
        active: usize,
        max: usize,
    },
    DepthExceeded,
    NotAvailable(String),
    ScriptSymlinkRejected {
        path: String,
    },
}

impl WorkflowToolError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "workflows_disabled",
            Self::InvalidInput(_) => "workflow_invalid_input",
            Self::NotFound { .. } => "workflow_not_found",
            Self::ResolveFailed(_) => "workflow_resolve_failed",
            Self::ValidationFailed(_) => "workflow_validation_failed",
            Self::ResumeUnknownRun { .. } => "workflow_resume_unknown_run",
            Self::LaunchFailed(_) => "workflow_launch_failed",
            Self::ControlUnknownRun { .. } => "workflow_control_unknown_run",
            Self::ControlNotApplicable { .. } => "workflow_control_not_applicable",
            Self::MaxActiveRunsExceeded { .. } => "workflow_max_active_runs_exceeded",
            Self::DepthExceeded => "workflow_depth_exceeded",
            Self::NotAvailable(_) => "workflow_not_available",
            Self::ScriptSymlinkRejected { .. } => "workflow_script_symlink_rejected",
        }
    }

    pub(crate) fn user_visible(&self) -> String {
        match self {
            Self::Disabled => "Background workflows are disabled. Enable them with \
                 `[features] workflows = true`."
                .to_string(),
            Self::InvalidInput(detail) => detail.clone(),
            Self::NotFound { name, available } => {
                let names = if available.is_empty() {
                    "<none>".to_string()
                } else {
                    available.join(", ")
                };
                format!("unknown workflow `{name}`; available: {names}")
            }
            Self::ResolveFailed(detail) => detail.clone(),
            Self::ValidationFailed(detail) => {
                format!("workflow validation failed: {detail}")
            }
            Self::ResumeUnknownRun { run_id, resumable } => {
                let names = if resumable.is_empty() {
                    "<none>".to_string()
                } else {
                    resumable.join(", ")
                };
                format!("cannot resume `{run_id}`; resumable runs: {names}")
            }
            Self::LaunchFailed(detail) => format!("workflow failed to launch: {detail}"),
            Self::ControlUnknownRun { run_id } => {
                format!("unknown workflow run `{run_id}`")
            }
            Self::ControlNotApplicable {
                run_id,
                status,
                allowed,
            } => format!(
                "workflow run `{run_id}` is {status:?}; allowed operations: {}",
                allowed.join(", ")
            ),
            Self::MaxActiveRunsExceeded { active, max } => format!(
                "too many active workflows ({active} > {max}); stop one before starting another"
            ),
            Self::DepthExceeded => "workflows can only be started by the root agent".to_string(),
            Self::NotAvailable(detail) => {
                format!("workflow runtime is not initialized: {detail}")
            }
            Self::ScriptSymlinkRejected { path } => {
                format!("refused to read workflow script at {path}: symlink or reparse point")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowControlEndpoint {
    // Resume currently flows through `RequestService::prepare`; the mapping
    // stays for when resume becomes a control action (deviation D-18).
    #[allow(dead_code)]
    Resume,
    Pause,
    Stop,
}

/// Map a typed control failure onto the shared tool code set. Capacity and
/// state transitions stay in the manager; this function only translates.
pub(crate) fn from_control(
    error: &WorkflowControlError,
    endpoint: WorkflowControlEndpoint,
) -> WorkflowToolError {
    match error {
        WorkflowControlError::Paths(_)
        | WorkflowControlError::Unavailable(_)
        | WorkflowControlError::Internal(_) => WorkflowToolError::NotAvailable(error.to_string()),
        WorkflowControlError::RunNotFound(run_id) => match endpoint {
            WorkflowControlEndpoint::Resume => WorkflowToolError::ResumeUnknownRun {
                run_id: run_id.clone(),
                resumable: Vec::new(),
            },
            WorkflowControlEndpoint::Pause | WorkflowControlEndpoint::Stop => {
                WorkflowToolError::ControlUnknownRun {
                    run_id: run_id.clone(),
                }
            }
        },
        WorkflowControlError::NotApplicable {
            run_id,
            status,
            allowed,
        } => WorkflowToolError::ControlNotApplicable {
            run_id: run_id.clone(),
            status: *status,
            allowed: allowed.clone(),
        },
        WorkflowControlError::MaxActiveRunsExceeded { active, max } => {
            WorkflowToolError::MaxActiveRunsExceeded {
                active: *active,
                max: *max,
            }
        }
        WorkflowControlError::Disabled => WorkflowToolError::Disabled,
        WorkflowControlError::LaunchFailed(detail) => {
            WorkflowToolError::LaunchFailed(detail.clone())
        }
        // Save-only variants never reach a control endpoint.
        WorkflowControlError::InvalidName(_)
        | WorkflowControlError::SaveExists(_)
        | WorkflowControlError::SaveFailed { .. } => {
            WorkflowToolError::NotAvailable(error.to_string())
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
