//! Pause/stop execution for the `workflow` tool.
//!
//! Capacity and state transitions live in `WorkflowManager`; this module only
//! maps typed failures onto the shared tool error contract.

use serde_json::{Value, json};

use crate::workflow::request_service::ControlAction;
use crate::workflow::run_manager::WorkflowManager;

use super::error::WorkflowControlEndpoint;
use super::error::WorkflowToolError;
use super::error::from_control;

fn endpoint_for(action: ControlAction) -> WorkflowControlEndpoint {
    match action {
        ControlAction::Pause => WorkflowControlEndpoint::Pause,
        ControlAction::Stop => WorkflowControlEndpoint::Stop,
    }
}

pub(crate) fn execute_control(
    manager: &WorkflowManager,
    action: ControlAction,
    run_id: &str,
) -> Result<Value, WorkflowToolError> {
    let snapshot = manager
        .control(action, run_id)
        .map_err(|error| from_control(&error, endpoint_for(action)))?;
    let verb = match action {
        ControlAction::Pause => "paused",
        ControlAction::Stop => "stopped",
    };
    let run_id_out = snapshot.run_id.clone();
    Ok(json!({
        "run_id": run_id_out,
        "task_id": snapshot.run_id,
        "name": snapshot.name,
        "status": snapshot.status.as_str(),
        "message": format!("workflow {verb}"),
    }))
}
