//! `workflow` tool handler.
//!
//! The handler validates the tagged request, resolves it through
//! `RequestService`, and either dry-runs (validate-only), launches, or applies a
//! pause/stop. A successful launch returns immediately; it never polls.

use serde_json::{Value, json};

use codex_features::Feature;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::workflow::RequestService;
use crate::workflow::WorkflowControlError;
use crate::workflow::WorkflowRequest;
use crate::workflow::manager_for_session;
use crate::workflow::registry::WorkflowRegistry;
use crate::workflow::request_service::PreparedRequest;
use crate::workflow::request_service::RequestError;
use crate::workflow::run_manager::WorkflowManager;

use super::control::execute_control;
use super::description::create_workflow_tool;
use super::error::WorkflowToolError;

pub(crate) struct WorkflowHandler;

impl WorkflowHandler {
    fn map_request_error(
        error: &RequestError,
        registry: &WorkflowRegistry,
        manager: &WorkflowManager,
    ) -> WorkflowToolError {
        let detail = error.detail().to_string();
        match error.code() {
            "workflows_disabled" => WorkflowToolError::Disabled,
            "workflow_not_found" => WorkflowToolError::NotFound {
                name: detail,
                available: registry
                    .list()
                    .into_iter()
                    .map(|entry| {
                        let mut label = format!("{} [{}]", entry.name, entry.scope);
                        if let Some(path) = entry.path {
                            label.push_str(&format!(" ({path})"));
                        }
                        if let Some(shadowed_by) = entry.shadowed_by {
                            label.push_str(&format!(" shadowed by {shadowed_by}"));
                        }
                        if let Some(when_to_use) = entry.when_to_use {
                            label.push_str(&format!(" - {when_to_use}"));
                        }
                        label.push_str(&format!(": {}", entry.description));
                        label
                    })
                    .collect(),
            },
            "workflow_resolve_failed" => WorkflowToolError::ResolveFailed(detail),
            "workflow_resume_unknown_run" => WorkflowToolError::ResumeUnknownRun {
                run_id: detail,
                resumable: manager.resumable(),
            },
            "workflow_control_unknown_run" => {
                WorkflowToolError::ControlUnknownRun { run_id: detail }
            }
            "workflow_script_symlink_rejected" => {
                WorkflowToolError::ScriptSymlinkRejected { path: detail }
            }
            "workflow_invalid_input" => WorkflowToolError::InvalidInput(detail),
            _ => WorkflowToolError::ResolveFailed(detail),
        }
    }
}

fn to_call_error(error: WorkflowToolError) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("{}: {}", error.code(), error.user_visible()))
}

fn map_control_error(error: &WorkflowControlError) -> WorkflowToolError {
    match error {
        WorkflowControlError::Disabled => WorkflowToolError::Disabled,
        WorkflowControlError::MaxActiveRunsExceeded { active, max } => {
            WorkflowToolError::MaxActiveRunsExceeded {
                active: *active,
                max: *max,
            }
        }
        WorkflowControlError::LaunchFailed(detail) => {
            WorkflowToolError::LaunchFailed(detail.clone())
        }
        WorkflowControlError::RunNotFound(run_id) => WorkflowToolError::ControlUnknownRun {
            run_id: run_id.clone(),
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
        WorkflowControlError::Paths(_)
        | WorkflowControlError::Unavailable(_)
        | WorkflowControlError::Internal(_)
        | WorkflowControlError::InvalidName(_)
        | WorkflowControlError::SaveExists(_)
        | WorkflowControlError::SaveFailed { .. } => {
            WorkflowToolError::NotAvailable(error.to_string())
        }
    }
}

fn validate(script: &str, args: Value, agent_budget: u64) -> Result<Value, WorkflowToolError> {
    match codex_workflow::validate_script_with_agent_budget(script, Some(args), agent_budget) {
        Ok(report) => Ok(json!({
            "valid": true,
            "name": report.name,
            "phases": report.phases,
            "outcome_ok": report.outcome_ok,
            "outcome_summary": report.outcome_summary,
        })),
        Err(codex_workflow::ValidationError::Meta(error)) => Err(
            WorkflowToolError::ValidationFailed(format!("META FAIL: {error}")),
        ),
        Err(codex_workflow::ValidationError::Run(error)) => Err(
            WorkflowToolError::ValidationFailed(format!("RUN FAIL: {error}")),
        ),
    }
}

impl ToolExecutor<ToolInvocation> for WorkflowHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(super::description::WORKFLOW_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_workflow_tool()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;

            if !turn.config.features.enabled(Feature::Workflows) {
                return Err(to_call_error(WorkflowToolError::Disabled));
            }
            if turn.session_source.get_agent_path().is_some() {
                return Err(to_call_error(WorkflowToolError::DepthExceeded));
            }

            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(
                    "workflow handler received unsupported payload".to_string(),
                ));
            };
            let request: WorkflowRequest = parse_arguments(&arguments)?;

            let manager = manager_for_session(&session)
                .await
                .map_err(|error| to_call_error(map_control_error(&error)))?;
            let registry = manager
                .registry()
                .map_err(|error| to_call_error(map_control_error(&error)))?;
            // The selected turn environment owns the cwd; the top-level
            // `turn.cwd` copy is deprecated.
            let cwd = turn
                .initial_environments
                .primary()
                .map(|environment| environment.cwd().to_path_buf())
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            let service = RequestService::new(
                &registry,
                manager.as_ref(),
                manager.paths().clone(),
                cwd,
                true,
            )
            .with_default_agent_budget(manager.limits().default_agent_budget);
            let prepared = service.prepare(&request).map_err(|error| {
                to_call_error(Self::map_request_error(&error, &registry, &manager))
            })?;

            let output = match prepared {
                PreparedRequest::Validate {
                    workflow,
                    args,
                    agent_budget,
                } => validate(&workflow.script, args, agent_budget).map_err(to_call_error)?,
                PreparedRequest::Launch {
                    workflow,
                    args,
                    agent_budget,
                    resume_run_id,
                } => {
                    let outcome = manager
                        .launch_prepared(workflow, args, agent_budget, true)
                        .map_err(|error| to_call_error(map_control_error(&error)))?;
                    let message = match resume_run_id {
                        Some(previous) => {
                            format!("{} (resumed from {previous})", outcome.message)
                        }
                        None => outcome.message,
                    };
                    json!({
                        "run_id": outcome.run_id,
                        "task_id": outcome.task_id,
                        "name": outcome.name,
                        "script_path": outcome.script_path,
                        "message": message,
                    })
                }
                PreparedRequest::Control { action, run_id } => {
                    execute_control(manager.as_ref(), action, &run_id).map_err(to_call_error)?
                }
            };

            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                serde_json::to_string(&output).unwrap_or_else(|error| {
                    format!("failed to serialize workflow result: {error}")
                }),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for WorkflowHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}
