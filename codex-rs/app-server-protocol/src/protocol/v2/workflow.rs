//! Experimental workflow notification and control requests.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowPhaseInfo {
    pub title: String,
    pub state: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentInfo {
    pub agent_id: String,
    pub label: String,
    pub phase: Option<String>,
    pub model: Option<String>,
    pub state: String,
    pub tokens_used: u64,
    pub duration_ms: u64,
}

/// Experimental snapshot notification for `workflow/updated`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowUpdatedNotification {
    pub thread_id: String,
    pub run_id: String,
    pub revision: u64,
    pub name: String,
    pub objective: String,
    pub status: String,
    pub foreground: bool,
    pub phases: Vec<WorkflowPhaseInfo>,
    pub current_phase: Option<String>,
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub agents_reserved: u64,
    pub agents_remaining: u64,
    pub agent_usage_incomplete: bool,
    pub elapsed_ms: u64,
    pub active_agents: u64,
    pub current_agent_label: Option<String>,
    pub agents: Vec<WorkflowAgentInfo>,
    pub last_event: Option<String>,
    pub last_event_detail: Option<String>,
    pub last_event_timestamp: Option<String>,
    pub pause_message: Option<String>,
    pub result_summary: Option<String>,
    pub degraded_fields: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListParams {
    pub thread_id: String,
    pub include_completed: bool,
}

/// Field order is fixed and maps one-to-one onto core's run summary.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunSummary {
    pub run_id: String,
    pub name: String,
    pub status: String,
    pub phase: Option<String>,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub agent_count: u32,
    pub script_path: String,
    pub run_dir: String,
    pub degraded_fields: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListResponse {
    pub runs: Vec<WorkflowRunSummary>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowPauseParams {
    pub thread_id: String,
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowResumeParams {
    pub thread_id: String,
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStopParams {
    pub thread_id: String,
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowControlResponse {
    pub run: WorkflowRunSummary,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowSaveParams {
    pub thread_id: String,
    pub run_id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowSaveResponse {
    pub path: String,
}

/// Seven shared codes plus the two app-server-only save extensions (D-10).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum WorkflowErrorCode {
    WorkflowsDisabled,
    WorkflowInvalidInput,
    WorkflowResumeUnknownRun,
    WorkflowControlUnknownRun,
    WorkflowControlNotApplicable,
    WorkflowMaxActiveRunsExceeded,
    WorkflowNotAvailable,
    WorkflowSaveExists,
    WorkflowSaveFailed,
}

impl WorkflowErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkflowsDisabled => "workflows_disabled",
            Self::WorkflowInvalidInput => "workflow_invalid_input",
            Self::WorkflowResumeUnknownRun => "workflow_resume_unknown_run",
            Self::WorkflowControlUnknownRun => "workflow_control_unknown_run",
            Self::WorkflowControlNotApplicable => "workflow_control_not_applicable",
            Self::WorkflowMaxActiveRunsExceeded => "workflow_max_active_runs_exceeded",
            Self::WorkflowNotAvailable => "workflow_not_available",
            Self::WorkflowSaveExists => "workflow_save_exists",
            Self::WorkflowSaveFailed => "workflow_save_failed",
        }
    }
}

/// Structured error payload carried in `JSONRPCErrorError.data`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowErrorData {
    pub code: WorkflowErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
