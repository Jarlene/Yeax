//! Snapshot-to-event mapping and the per-session broadcast forwarder.
//!
//! The manager owns state transitions; this module only translates snapshots
//! into `EventMsg::WorkflowUpdated` and drops stale or repeated revisions.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::DateTime;
use chrono::SecondsFormat;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::workflow::WorkflowAgentInfo;
use codex_protocol::workflow::WorkflowPhaseInfo;
use codex_protocol::workflow::WorkflowUpdatedEvent;

use super::run_manager::WorkflowManager;
use super::tracker::AgentPhase;
use super::tracker::WorkflowSnapshot;

/// Per-run revision gate shared by every forwarder.
#[derive(Debug, Default)]
pub(crate) struct RevisionGate {
    seen: HashMap<String, u64>,
    terminal_sent: HashSet<String>,
}

impl RevisionGate {
    /// A snapshot is forwarded only when it is newer and not a repeated terminal.
    pub(crate) fn accept(&mut self, snapshot: &WorkflowSnapshot) -> bool {
        if snapshot.status.is_terminal() && self.terminal_sent.contains(&snapshot.run_id) {
            return false;
        }
        let previous = self.seen.get(&snapshot.run_id).copied();
        if previous.is_some_and(|seen| snapshot.revision <= seen) {
            return false;
        }
        self.seen.insert(snapshot.run_id.clone(), snapshot.revision);
        if snapshot.status.is_terminal() {
            self.terminal_sent.insert(snapshot.run_id.clone());
        }
        true
    }
}

fn phase_state(index: usize, phases: &[String], current: Option<&str>) -> String {
    let Some(current) = current else {
        return "pending".to_string();
    };
    let Some(current_index) = phases.iter().position(|phase| phase == current) else {
        return "pending".to_string();
    };
    match index.cmp(&current_index) {
        std::cmp::Ordering::Less => "complete".to_string(),
        std::cmp::Ordering::Equal => "active".to_string(),
        std::cmp::Ordering::Greater => "pending".to_string(),
    }
}

fn agent_state(phase: AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Running => "running",
        AgentPhase::Completed => "completed",
        AgentPhase::Failed => "failed",
        AgentPhase::Cancelled => "cancelled",
    }
}

fn timestamp(ms: Option<u64>) -> Option<String> {
    ms.and_then(|value| i64::try_from(value).ok())
        .and_then(DateTime::from_timestamp_millis)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// Translate one tracker snapshot into the wire event.
pub(crate) fn snapshot_to_event(
    thread_id: ThreadId,
    snapshot: &WorkflowSnapshot,
    now_ms: u64,
) -> WorkflowUpdatedEvent {
    let phases: Vec<WorkflowPhaseInfo> = snapshot
        .phases
        .iter()
        .enumerate()
        .map(|(index, title)| WorkflowPhaseInfo {
            title: title.clone(),
            state: phase_state(index, &snapshot.phases, snapshot.current_phase.as_deref()),
        })
        .collect();

    let mut degraded_fields = snapshot.degraded_fields.clone();
    if !snapshot.agents.is_empty() && !degraded_fields.iter().any(|field| field == "agents.model") {
        degraded_fields.push("agents.model".to_string());
    }

    let agents: Vec<WorkflowAgentInfo> = snapshot
        .agents
        .iter()
        .map(|agent| {
            let finished = agent.finished_at_ms.unwrap_or(now_ms);
            WorkflowAgentInfo {
                agent_id: agent.agent_id.clone(),
                label: agent.label.clone(),
                phase: agent.workflow_phase.clone(),
                model: None,
                state: agent_state(agent.phase).to_string(),
                tokens_used: agent.tokens_used,
                duration_ms: finished.saturating_sub(agent.started_at_ms),
            }
        })
        .collect();

    WorkflowUpdatedEvent {
        thread_id,
        run_id: snapshot.run_id.clone(),
        revision: snapshot.revision,
        name: snapshot.name.clone(),
        objective: snapshot.objective.clone(),
        status: snapshot.status.as_str().to_string(),
        foreground: snapshot.foreground,
        phases,
        current_phase: snapshot.current_phase.clone(),
        agent_budget: snapshot.budget.total,
        agents_used: snapshot.budget.spent,
        agents_reserved: snapshot.budget.reserved,
        agents_remaining: snapshot.budget.remaining.unwrap_or(0),
        agent_usage_incomplete: snapshot.budget.usage_incomplete,
        elapsed_ms: snapshot.elapsed_ms,
        active_agents: u64::try_from(snapshot.active_agents).unwrap_or(u64::MAX),
        current_agent_label: snapshot.current_agent_label.clone(),
        agents,
        last_event: snapshot.last_event.clone(),
        last_event_detail: snapshot.last_event_detail.clone(),
        last_event_timestamp: timestamp(snapshot.last_event_at_ms),
        pause_message: snapshot.pause_message.clone(),
        result_summary: snapshot.result_summary.clone(),
        degraded_fields,
    }
}

/// Forward manager broadcasts to the session event stream until it closes.
pub(crate) fn spawn_event_forwarder(
    session: Arc<crate::session::session::Session>,
    manager: Arc<WorkflowManager>,
) {
    let thread_id = session.thread_id();
    let mut receiver = manager.subscribe();
    tokio::spawn(async move {
        let mut gate = RevisionGate::default();
        loop {
            match receiver.recv().await {
                Ok(snapshot) => {
                    if !gate.accept(&snapshot) {
                        continue;
                    }
                    let event = Event {
                        id: snapshot.run_id.clone(),
                        msg: EventMsg::WorkflowUpdated(snapshot_to_event(
                            thread_id,
                            &snapshot,
                            snapshot.elapsed_ms,
                        )),
                    };
                    session.send_event_ephemeral(event).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Test-only status reachability helper kept next to the mapping.
#[cfg(test)]
use super::tracker::RunStatus;

#[cfg(test)]
pub(crate) fn is_known_status(status: RunStatus) -> bool {
    RunStatus::ALL.contains(&status)
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
