//! Workflow run state machine, agent rows, and revision tracking.
//!
//! The tracker owns the eleven wire-visible statuses defined by the design. It
//! is deliberately free of async runtime and I/O so it can be unit tested and
//! driven from the manager while holding a single lock.

use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

/// Maximum retained history rows per run.
pub(crate) const MAX_HISTORY_ROWS: usize = 200;
/// Maximum retained agent rows per run (oldest terminal rows drop first).
pub(crate) const MAX_AGENT_ROWS: usize = 64;
/// Maximum length of one history/event message.
pub(crate) const MAX_EVENT_MESSAGE_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Active,
    UserPaused,
    BackOffPaused,
    NoProgressPaused,
    InfraPaused,
    Blocked,
    BudgetLimited,
    Interrupted,
    Complete,
    Failed,
    Cancelled,
}

impl RunStatus {
    // Enumerated for exhaustive status handling in tests (deviation D-14).
    #[allow(dead_code)]
    pub(crate) const ALL: [Self; 11] = [
        Self::Active,
        Self::UserPaused,
        Self::BackOffPaused,
        Self::NoProgressPaused,
        Self::InfraPaused,
        Self::Blocked,
        Self::BudgetLimited,
        Self::Interrupted,
        Self::Complete,
        Self::Failed,
        Self::Cancelled,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::UserPaused => "user_paused",
            Self::BackOffPaused => "back_off_paused",
            Self::NoProgressPaused => "no_progress_paused",
            Self::InfraPaused => "infra_paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget_limited",
            Self::Interrupted => "interrupted",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Terminal statuses are reported once and never transition again.
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }

    /// `budget_limited` is a reportable completion but can be resumed with a
    /// larger budget, so it is not terminal.
    #[allow(dead_code)]
    pub(crate) fn is_reportable_completion(self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Failed | Self::Cancelled | Self::BudgetLimited
        )
    }

    pub(crate) fn is_paused(self) -> bool {
        matches!(
            self,
            Self::UserPaused
                | Self::BackOffPaused
                | Self::NoProgressPaused
                | Self::InfraPaused
                | Self::Blocked
        )
    }

    pub(crate) fn can_pause(self) -> bool {
        matches!(self, Self::Active)
    }

    pub(crate) fn can_stop(self) -> bool {
        !self.is_terminal()
    }

    pub(crate) fn can_resume(self) -> bool {
        self.is_paused() || matches!(self, Self::BudgetLimited | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PauseReason {
    User,
    BackOff,
    NoProgress,
    Infra,
    Verification,
}

impl PauseReason {
    pub(crate) fn status(self) -> RunStatus {
        match self {
            Self::User => RunStatus::UserPaused,
            Self::BackOff => RunStatus::BackOffPaused,
            Self::NoProgress => RunStatus::NoProgressPaused,
            Self::Infra => RunStatus::InfraPaused,
            Self::Verification => RunStatus::Blocked,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackerError {
    InvalidTransition {
        from: RunStatus,
        action: &'static str,
    },
    ResumeRequiresHigherBudget {
        current: u64,
        requested: u64,
    },
    HistoryMessageTooLong {
        actual: usize,
        max: usize,
    },
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTransition { from, action } => {
                write!(
                    f,
                    "cannot {action} a workflow run in status `{}`",
                    from.as_str()
                )
            }
            Self::ResumeRequiresHigherBudget { current, requested } => write!(
                f,
                "resume requires a higher agent budget (current {current}, requested {requested})"
            ),
            Self::HistoryMessageTooLong { actual, max } => {
                write!(
                    f,
                    "workflow event message is {actual} bytes, maximum is {max}"
                )
            }
        }
    }
}

impl std::error::Error for TrackerError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentPhase {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AgentRow {
    pub agent_id: String,
    pub label: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub workflow_phase: Option<String>,
    pub started_at_ms: u64,
    #[serde(default)]
    pub finished_at_ms: Option<u64>,
    #[serde(default)]
    pub tokens_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryRow {
    pub at_ms: u64,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BudgetView {
    pub total: Option<u64>,
    pub spent: u64,
    pub reserved: u64,
    pub remaining: Option<u64>,
    pub usage_incomplete: bool,
}

impl BudgetView {
    fn recompute_remaining(&mut self) {
        self.remaining = self.total.map(|total| {
            total
                .saturating_sub(self.spent)
                .saturating_sub(self.reserved)
        });
    }
}

/// Public snapshot consumed by protocol events and the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkflowSnapshot {
    pub run_id: String,
    pub name: String,
    pub objective: String,
    pub status: RunStatus,
    pub foreground: bool,
    pub revision: u64,
    pub phases: Vec<String>,
    pub current_phase: Option<String>,
    pub budget: BudgetView,
    pub elapsed_ms: u64,
    pub active_agents: usize,
    pub current_agent_label: Option<String>,
    pub agents: Vec<AgentRow>,
    pub history: Vec<HistoryRow>,
    pub last_event: Option<String>,
    pub last_event_detail: Option<String>,
    pub last_event_at_ms: Option<u64>,
    pub pause_message: Option<String>,
    pub result_summary: Option<String>,
    pub degraded_fields: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Tracker {
    run_id: String,
    name: String,
    objective: String,
    status: RunStatus,
    foreground: bool,
    revision: u64,
    phases: Vec<String>,
    current_phase: Option<String>,
    budget: BudgetView,
    agents: BTreeMap<String, AgentRow>,
    agent_order: Vec<String>,
    history: Vec<HistoryRow>,
    last_event: Option<String>,
    last_event_detail: Option<String>,
    last_event_at_ms: Option<u64>,
    pause_message: Option<String>,
    result_summary: Option<String>,
    degraded_fields: Vec<String>,
    started: Instant,
    clock_ms: u64,
}

impl Tracker {
    pub(crate) fn new(
        run_id: impl Into<String>,
        name: impl Into<String>,
        objective: impl Into<String>,
        phases: Vec<String>,
        agent_budget: Option<u64>,
        foreground: bool,
        started_ms: u64,
    ) -> Self {
        let mut budget = BudgetView {
            total: agent_budget,
            spent: 0,
            reserved: 0,
            remaining: agent_budget,
            usage_incomplete: false,
        };
        budget.recompute_remaining();
        Self {
            run_id: run_id.into(),
            name: name.into(),
            objective: objective.into(),
            status: RunStatus::Active,
            foreground,
            revision: 0,
            phases,
            current_phase: None,
            budget,
            agents: BTreeMap::new(),
            agent_order: Vec::new(),
            history: Vec::new(),
            last_event: None,
            last_event_detail: None,
            last_event_at_ms: None,
            pause_message: None,
            result_summary: None,
            degraded_fields: Vec::new(),
            started: Instant::now(),
            clock_ms: started_ms,
        }
    }

    pub(crate) fn status(&self) -> RunStatus {
        self.status
    }

    // Reserved transition helpers used by tracker tests. Deviation D-14.
    #[allow(dead_code)]
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Wall-clock start time captured by `Tracker::new`.
    pub(crate) fn started_at_ms(&self) -> u64 {
        self.clock_ms
    }

    #[allow(dead_code)]
    pub(crate) fn tick(&mut self, now_ms: u64) {
        self.clock_ms = self.clock_ms.max(now_ms);
    }

    #[allow(dead_code)]
    pub(crate) fn note_degraded(&mut self, field: impl Into<String>) {
        let field = field.into();
        if !self.degraded_fields.contains(&field) {
            self.degraded_fields.push(field);
        }
    }

    pub(crate) fn set_phase(&mut self, title: impl Into<String>) -> Result<(), TrackerError> {
        let title = title.into();
        if !self.phases.contains(&title) {
            self.phases.push(title.clone());
        }
        self.current_phase = Some(title.clone());
        self.push_event("phase", title, Some("phase changed".to_string()))
    }

    pub(crate) fn reserve_agents(&mut self, count: u64) -> Result<(), TrackerError> {
        self.budget.reserved = self.budget.reserved.saturating_add(count);
        self.budget.recompute_remaining();
        self.touch();
        Ok(())
    }

    pub(crate) fn release_agents(&mut self, count: u64) {
        self.budget.reserved = self.budget.reserved.saturating_sub(count);
        self.budget.recompute_remaining();
        self.touch();
    }

    pub(crate) fn agent_started(
        &mut self,
        agent_id: impl Into<String>,
        label: impl Into<String>,
        workflow_phase: Option<String>,
        now_ms: u64,
    ) {
        let agent_id = agent_id.into();
        let label = label.into();
        if !self.agents.contains_key(&agent_id) {
            self.agent_order.push(agent_id.clone());
        }
        self.agents.insert(
            agent_id.clone(),
            AgentRow {
                agent_id,
                label: label.clone(),
                phase: AgentPhase::Running,
                workflow_phase,
                started_at_ms: now_ms,
                finished_at_ms: None,
                tokens_used: 0,
            },
        );
        self.budget.spent = self.budget.spent.saturating_add(1);
        self.budget.reserved = self.budget.reserved.saturating_sub(1);
        self.budget.recompute_remaining();
        self.trim_agents();
        self.push_event("agent_started", label, None).ok();
    }

    #[allow(dead_code)]
    pub(crate) fn agent_finished(
        &mut self,
        agent_id: &str,
        phase: AgentPhase,
        tokens_used: u64,
        now_ms: u64,
    ) {
        if let Some(row) = self.agents.get_mut(agent_id) {
            row.phase = phase;
            row.finished_at_ms = Some(now_ms);
            row.tokens_used = tokens_used;
            let label = row.label.clone();
            self.push_event("agent_finished", label, None).ok();
        }
    }

    pub(crate) fn push_event(
        &mut self,
        kind: &str,
        message: impl Into<String>,
        detail: Option<String>,
    ) -> Result<(), TrackerError> {
        let message = message.into();
        if message.len() > MAX_EVENT_MESSAGE_BYTES {
            return Err(TrackerError::HistoryMessageTooLong {
                actual: message.len(),
                max: MAX_EVENT_MESSAGE_BYTES,
            });
        }
        let at_ms = self.clock_ms;
        self.history.push(HistoryRow {
            at_ms,
            kind: kind.to_string(),
            message: message.clone(),
        });
        if self.history.len() > MAX_HISTORY_ROWS {
            let excess = self.history.len() - MAX_HISTORY_ROWS;
            self.history.drain(0..excess);
        }
        self.last_event = Some(message);
        self.last_event_detail = detail;
        self.last_event_at_ms = Some(at_ms);
        self.touch();
        Ok(())
    }

    pub(crate) fn pause(
        &mut self,
        reason: PauseReason,
        message: impl Into<String>,
    ) -> Result<(), TrackerError> {
        if !self.status.can_pause() {
            return Err(TrackerError::InvalidTransition {
                from: self.status,
                action: "pause",
            });
        }
        let message = message.into();
        self.status = reason.status();
        self.pause_message = Some(message.clone());
        self.push_event(
            "paused",
            message,
            Some(reason.status().as_str().to_string()),
        )?;
        Ok(())
    }

    pub(crate) fn resume(&mut self, agent_budget: Option<u64>) -> Result<(), TrackerError> {
        if !self.status.can_resume() {
            return Err(TrackerError::InvalidTransition {
                from: self.status,
                action: "resume",
            });
        }
        if self.status == RunStatus::BudgetLimited {
            let current = self.budget.total.unwrap_or(0);
            let requested = agent_budget.ok_or(TrackerError::ResumeRequiresHigherBudget {
                current,
                requested: current,
            })?;
            if requested <= current {
                return Err(TrackerError::ResumeRequiresHigherBudget { current, requested });
            }
            self.budget.total = Some(requested);
            self.budget.recompute_remaining();
        }
        self.status = RunStatus::Active;
        self.pause_message = None;
        self.push_event("resumed", "workflow resumed", None)?;
        Ok(())
    }

    pub(crate) fn stop(&mut self) -> Result<(), TrackerError> {
        if !self.status.can_stop() {
            return Err(TrackerError::InvalidTransition {
                from: self.status,
                action: "stop",
            });
        }
        self.status = RunStatus::Cancelled;
        self.push_event("cancelled", "workflow cancelled", None)?;
        Ok(())
    }

    pub(crate) fn complete(&mut self, summary: Option<String>) -> Result<(), TrackerError> {
        self.finish(RunStatus::Complete, "complete", summary)
    }

    pub(crate) fn fail(&mut self, summary: Option<String>) -> Result<(), TrackerError> {
        self.finish(RunStatus::Failed, "failed", summary)
    }

    pub(crate) fn budget_limited(&mut self, summary: Option<String>) -> Result<(), TrackerError> {
        if self.status.is_terminal() {
            return Err(TrackerError::InvalidTransition {
                from: self.status,
                action: "limit budget",
            });
        }
        self.status = RunStatus::BudgetLimited;
        self.result_summary = summary;
        self.push_event("budget_limited", "agent budget exhausted", None)
    }

    #[allow(dead_code)]
    pub(crate) fn mark_interrupted(&mut self) -> Result<(), TrackerError> {
        if self.status.is_terminal() {
            return Err(TrackerError::InvalidTransition {
                from: self.status,
                action: "interrupt",
            });
        }
        self.status = RunStatus::Interrupted;
        self.push_event(
            "interrupted",
            "workflow interrupted by process restart",
            None,
        )
    }

    fn finish(
        &mut self,
        status: RunStatus,
        kind: &str,
        summary: Option<String>,
    ) -> Result<(), TrackerError> {
        // Duplicate terminal reports keep metrics but never change the state.
        if self.status.is_terminal() {
            return Ok(());
        }
        self.status = status;
        self.result_summary = summary.clone();
        self.push_event(kind, summary.unwrap_or_else(|| kind.to_string()), None)
    }

    pub(crate) fn snapshot(&self) -> WorkflowSnapshot {
        let agents: Vec<AgentRow> = self
            .agent_order
            .iter()
            .filter_map(|id| self.agents.get(id).cloned())
            .collect();
        let active_agents = agents
            .iter()
            .filter(|row| row.phase == AgentPhase::Running)
            .count();
        WorkflowSnapshot {
            run_id: self.run_id.clone(),
            name: self.name.clone(),
            objective: self.objective.clone(),
            status: self.status,
            foreground: self.foreground,
            revision: self.revision,
            phases: self.phases.clone(),
            current_phase: self.current_phase.clone(),
            budget: self.budget.clone(),
            elapsed_ms: duration_ms(self.started.elapsed()),
            active_agents,
            current_agent_label: agents
                .iter()
                .rev()
                .find(|row| row.phase == AgentPhase::Running)
                .map(|row| row.label.clone()),
            agents,
            history: self.history.clone(),
            last_event: self.last_event.clone(),
            last_event_detail: self.last_event_detail.clone(),
            last_event_at_ms: self.last_event_at_ms,
            pause_message: self.pause_message.clone(),
            result_summary: self.result_summary.clone(),
            degraded_fields: self.degraded_fields.clone(),
        }
    }

    fn trim_agents(&mut self) {
        if self.agent_order.len() <= MAX_AGENT_ROWS {
            return;
        }
        let mut removable: Vec<String> = self
            .agent_order
            .iter()
            .filter(|id| {
                self.agents
                    .get(*id)
                    .is_some_and(|row| row.phase != AgentPhase::Running)
            })
            .cloned()
            .collect();
        let excess = self.agent_order.len() - MAX_AGENT_ROWS;
        removable.truncate(excess);
        for id in removable {
            self.agents.remove(&id);
            self.agent_order.retain(|candidate| candidate != &id);
        }
    }

    fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "tracker_tests.rs"]
mod tests;
