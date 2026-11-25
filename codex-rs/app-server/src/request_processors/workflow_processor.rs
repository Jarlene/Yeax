//! Experimental app-server surface for background workflows.
//!
//! Every request resolves the session-scoped manager from the requested
//! `thread_id`; the app-server never keeps run state of its own.

use super::*;
use crate::error_code::method_not_found;
use codex_app_server_protocol::WorkflowControlResponse;
use codex_app_server_protocol::WorkflowErrorCode;
use codex_app_server_protocol::WorkflowErrorData;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowPauseParams;
use codex_app_server_protocol::WorkflowResumeParams;
use codex_app_server_protocol::WorkflowRunSummary;
use codex_app_server_protocol::WorkflowSaveParams;
use codex_app_server_protocol::WorkflowSaveResponse;
use codex_app_server_protocol::WorkflowStopParams;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_core::workflow::WorkflowControlError;
use codex_core::workflow::WorkflowManager;
use codex_core::workflow::WorkflowRunSummary as CoreRunSummary;
use codex_core::workflow::manager_for_thread;
use codex_features::Feature;
use codex_protocol::ThreadId;

/// Control endpoints differ only in how `RunNotFound` and `Internal` map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowEndpoint {
    List,
    Pause,
    Resume,
    Stop,
    Save,
}

#[derive(Clone)]
pub(crate) struct WorkflowRequestProcessor {
    thread_manager: Arc<ThreadManager>,
}

impl WorkflowRequestProcessor {
    pub(crate) fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }

    pub(crate) async fn workflow_list(
        &self,
        params: WorkflowListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, manager) = self.resolve(&params.thread_id).await?;
        let runs = manager
            .list_runs(params.include_completed)
            .into_iter()
            .map(to_dto)
            .collect();
        Ok(Some(WorkflowListResponse { runs }.into()))
    }

    pub(crate) async fn workflow_pause(
        &self,
        params: WorkflowPauseParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, manager) = self.resolve(&params.thread_id).await?;
        let run = manager
            .pause_run(&params.run_id)
            .map(to_dto)
            .map_err(|error| map_control_error(&error, WorkflowEndpoint::Pause))?;
        Ok(Some(ClientResponsePayload::WorkflowPause(
            WorkflowControlResponse { run },
        )))
    }

    pub(crate) async fn workflow_resume(
        &self,
        params: WorkflowResumeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, manager) = self.resolve(&params.thread_id).await?;
        let run = manager
            .resume_run(&params.run_id)
            .map(to_dto)
            .map_err(|error| map_control_error(&error, WorkflowEndpoint::Resume))?;
        Ok(Some(ClientResponsePayload::WorkflowResume(
            WorkflowControlResponse { run },
        )))
    }

    pub(crate) async fn workflow_stop(
        &self,
        params: WorkflowStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, manager) = self.resolve(&params.thread_id).await?;
        let run = manager
            .stop_run(&params.run_id)
            .map(to_dto)
            .map_err(|error| map_control_error(&error, WorkflowEndpoint::Stop))?;
        Ok(Some(ClientResponsePayload::WorkflowStop(
            WorkflowControlResponse { run },
        )))
    }

    pub(crate) async fn workflow_save(
        &self,
        params: WorkflowSaveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, manager) = self.resolve(&params.thread_id).await?;
        let path = manager
            .save_user_workflow(&params.run_id, &params.name)
            .map_err(|error| map_control_error(&error, WorkflowEndpoint::Save))?;
        Ok(Some(
            WorkflowSaveResponse {
                path: path.display().to_string(),
            }
            .into(),
        ))
    }

    /// `thread_id` is the only routing key; invalid UUIDs and unknown ids are
    /// indistinguishable to the client (`workflow_invalid_input`).
    async fn resolve(
        &self,
        thread_id: &str,
    ) -> Result<(Arc<CodexThread>, Arc<WorkflowManager>), JSONRPCErrorError> {
        let parsed = ThreadId::from_string(thread_id).map_err(|_| thread_not_found(thread_id))?;
        let thread = self
            .thread_manager
            .get_thread(parsed)
            .await
            .map_err(|_| thread_not_found(thread_id))?;
        if !thread.enabled(Feature::Workflows) {
            return Err(workflow_error(
                WorkflowErrorCode::WorkflowsDisabled,
                "workflows are disabled; enable `[features] workflows = true`",
                None,
            ));
        }
        let manager = manager_for_thread(&thread)
            .await
            .map_err(|error| map_control_error(&error, WorkflowEndpoint::List))?;
        Ok((thread, manager))
    }
}

fn thread_not_found(thread_id: &str) -> JSONRPCErrorError {
    workflow_error(
        WorkflowErrorCode::WorkflowInvalidInput,
        "workflow thread not found",
        Some(thread_id.to_string()),
    )
}

fn to_dto(summary: CoreRunSummary) -> WorkflowRunSummary {
    WorkflowRunSummary {
        run_id: summary.run_id,
        name: summary.name,
        status: summary.status,
        phase: summary.phase,
        started_at_ms: summary.started_at_ms,
        updated_at_ms: summary.updated_at_ms,
        agent_count: summary.agent_count,
        script_path: summary.script_path.display().to_string(),
        run_dir: summary.run_dir.display().to_string(),
        degraded_fields: summary.degraded_fields,
    }
}

/// Map the shared typed control errors onto the app-server error contract.
pub(crate) fn map_control_error(
    error: &WorkflowControlError,
    endpoint: WorkflowEndpoint,
) -> JSONRPCErrorError {
    match error {
        WorkflowControlError::Paths(_) | WorkflowControlError::Unavailable(_) => workflow_error(
            WorkflowErrorCode::WorkflowNotAvailable,
            "workflow not available",
            Some(error.to_string()),
        ),
        WorkflowControlError::Internal(detail) => match endpoint {
            WorkflowEndpoint::Save => workflow_error(
                WorkflowErrorCode::WorkflowSaveFailed,
                "workflow save failed",
                Some(detail.clone()),
            ),
            _ => workflow_error(
                WorkflowErrorCode::WorkflowNotAvailable,
                "workflow not available",
                Some(detail.clone()),
            ),
        },
        WorkflowControlError::Disabled => workflow_error(
            WorkflowErrorCode::WorkflowsDisabled,
            "workflows are disabled; enable `[features] workflows = true`",
            None,
        ),
        WorkflowControlError::LaunchFailed(detail) => workflow_error(
            WorkflowErrorCode::WorkflowNotAvailable,
            "workflow not available",
            Some(detail.clone()),
        ),
        WorkflowControlError::RunNotFound(run_id) => match endpoint {
            WorkflowEndpoint::Resume => workflow_error(
                WorkflowErrorCode::WorkflowResumeUnknownRun,
                "workflow unknown run",
                Some(run_id.clone()),
            ),
            WorkflowEndpoint::Pause | WorkflowEndpoint::Stop => workflow_error(
                WorkflowErrorCode::WorkflowControlUnknownRun,
                "workflow unknown run",
                Some(run_id.clone()),
            ),
            WorkflowEndpoint::List | WorkflowEndpoint::Save => workflow_error(
                WorkflowErrorCode::WorkflowInvalidInput,
                "workflow unknown run",
                Some(run_id.clone()),
            ),
        },
        WorkflowControlError::NotApplicable {
            run_id,
            status,
            allowed,
        } => workflow_error(
            WorkflowErrorCode::WorkflowControlNotApplicable,
            "workflow control not applicable",
            Some(format!(
                "{run_id} is {status:?}; allowed: {}",
                allowed.join(", ")
            )),
        ),
        WorkflowControlError::MaxActiveRunsExceeded { active, max } => workflow_error(
            WorkflowErrorCode::WorkflowMaxActiveRunsExceeded,
            "workflow max active runs exceeded",
            Some(format!("{active} > {max}")),
        ),
        WorkflowControlError::InvalidName(name) => workflow_error(
            WorkflowErrorCode::WorkflowInvalidInput,
            "workflow name invalid",
            Some(name.clone()),
        ),
        WorkflowControlError::SaveExists(path) => workflow_error(
            WorkflowErrorCode::WorkflowSaveExists,
            "workflow already exists",
            Some(path.display().to_string()),
        ),
        WorkflowControlError::SaveFailed { path, detail } => workflow_error(
            WorkflowErrorCode::WorkflowSaveFailed,
            "workflow save failed",
            Some(format!("{}: {detail}", path.display())),
        ),
    }
}

fn workflow_error(
    code: WorkflowErrorCode,
    message: &str,
    detail: Option<String>,
) -> JSONRPCErrorError {
    // `WorkflowErrorData` only holds a fieldless enum and an `Option<String>`,
    // so serialization is total in practice. Keep the error path explicit
    // instead of panicking inside an RPC handler.
    let data = match serde_json::to_value(WorkflowErrorData { code, detail }) {
        Ok(value) => value,
        Err(error) => serde_json::json!({
            "code": code,
            "detail": format!("workflow error data serialization failed: {error}"),
        }),
    };
    match code {
        WorkflowErrorCode::WorkflowsDisabled => {
            let mut error = method_not_found(message);
            error.data = Some(data);
            error
        }
        WorkflowErrorCode::WorkflowNotAvailable | WorkflowErrorCode::WorkflowSaveFailed => {
            let mut error = internal_error(message);
            error.data = Some(data);
            error
        }
        _ => {
            let mut error = invalid_params(message);
            error.data = Some(data);
            error
        }
    }
}

#[cfg(test)]
#[path = "workflow_processor_tests.rs"]
mod tests;
