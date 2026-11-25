//! Wire contract for workflow run updates.
//!
//! The event is intentionally a snapshot: clients render the newest revision
//! they have seen and ignore anything older. It never carries script bodies,
//! prompts, tokens, or credentials.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

use crate::ThreadId;

/// Wire-visible run statuses. Terminal statuses are broadcast once.
pub const WORKFLOW_STATUSES: [&str; 11] = [
    "active",
    "user_paused",
    "back_off_paused",
    "no_progress_paused",
    "infra_paused",
    "blocked",
    "budget_limited",
    "interrupted",
    "complete",
    "failed",
    "cancelled",
];

/// One declared phase and its display state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhaseInfo {
    pub title: String,
    pub state: String,
}

/// One agent row belonging to a run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentInfo {
    pub agent_id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model: Option<String>,
    pub state: String,
    pub tokens_used: u64,
    pub duration_ms: u64,
}

/// Snapshot event emitted by the workflow manager and routed by `thread_id`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowUpdatedEvent {
    pub thread_id: ThreadId,
    pub run_id: String,
    pub revision: u64,
    pub name: String,
    pub objective: String,
    pub status: String,
    pub foreground: bool,
    #[serde(default)]
    pub phases: Vec<WorkflowPhaseInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub current_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub agent_budget: Option<u64>,
    pub agents_used: u64,
    pub agents_reserved: u64,
    pub agents_remaining: u64,
    pub agent_usage_incomplete: bool,
    pub elapsed_ms: u64,
    pub active_agents: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub current_agent_label: Option<String>,
    #[serde(default)]
    pub agents: Vec<WorkflowAgentInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_event_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_event_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pause_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result_summary: Option<String>,
    #[serde(default)]
    pub degraded_fields: Vec<String>,
}

impl WorkflowUpdatedEvent {
    /// Unknown statuses are never invented by clients; they render as degraded.
    pub fn is_known_status(&self) -> bool {
        WORKFLOW_STATUSES.contains(&self.status.as_str())
    }

    /// Terminal statuses are broadcast once and never transition again.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "complete" | "failed" | "cancelled")
    }

    /// Clients only accept a strictly larger revision for the same run.
    pub fn should_apply(&self, seen_revision: Option<u64>) -> bool {
        match seen_revision {
            Some(seen) => self.revision > seen,
            None => true,
        }
    }
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
