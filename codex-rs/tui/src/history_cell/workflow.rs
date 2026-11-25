//! Workflow history cards.
//!
//! Cards are keyed by `run_id` and only accept strictly newer revisions, so a
//! delayed notification cannot roll a run back. `cleared` tombstones make a run
//! disappear for the rest of the session even if a stale update arrives later.
//! Elapsed time for active runs is smoothed locally from `received_at`.

use super::HistoryCell;
use codex_app_server_protocol::WorkflowRunSummary;
use codex_app_server_protocol::WorkflowUpdatedNotification;
use ratatui::text::Line;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Instant;

/// Terminal statuses are broadcast once; clients must not keep smoothing them.
pub(crate) const TERMINAL_STATUSES: [&str; 3] = ["complete", "failed", "cancelled"];

pub(crate) fn is_terminal_status(status: &str) -> bool {
    TERMINAL_STATUSES.contains(&status)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkflowCard {
    pub run_id: String,
    pub name: String,
    pub status: String,
    pub phase: Option<String>,
    pub agents_used: u64,
    pub agents_reserved: u64,
    pub agent_budget: Option<u64>,
    pub current_agent_label: Option<String>,
    pub last_event: Option<String>,
    pub last_event_detail: Option<String>,
    pub result_summary: Option<String>,
    pub pause_message: Option<String>,
    pub degraded_fields: Vec<String>,
    pub revision: u64,
    /// Elapsed time reported by the server when this snapshot was produced.
    pub elapsed_ms: u64,
    /// Local receive time used to smooth active-run elapsed time.
    pub received_at: Instant,
}

impl WorkflowCard {
    pub(crate) fn from_notification(
        notification: &WorkflowUpdatedNotification,
        received_at: Instant,
    ) -> Self {
        Self {
            run_id: notification.run_id.clone(),
            name: notification.name.clone(),
            status: notification.status.clone(),
            phase: notification.current_phase.clone(),
            agents_used: notification.agents_used,
            agents_reserved: notification.agents_reserved,
            agent_budget: notification.agent_budget,
            current_agent_label: notification.current_agent_label.clone(),
            last_event: notification.last_event.clone(),
            last_event_detail: notification.last_event_detail.clone(),
            result_summary: notification.result_summary.clone(),
            pause_message: notification.pause_message.clone(),
            degraded_fields: notification.degraded_fields.clone(),
            revision: notification.revision,
            elapsed_ms: notification.elapsed_ms,
            received_at,
        }
    }

    pub(crate) fn from_summary(summary: &WorkflowRunSummary, received_at: Instant) -> Self {
        let elapsed_ms = summary
            .updated_at_ms
            .saturating_sub(summary.started_at_ms)
            .max(0) as u64;
        Self {
            run_id: summary.run_id.clone(),
            name: summary.name.clone(),
            status: summary.status.clone(),
            phase: summary.phase.clone(),
            agents_used: u64::from(summary.agent_count),
            agents_reserved: 0,
            agent_budget: None,
            current_agent_label: None,
            last_event: None,
            last_event_detail: None,
            result_summary: None,
            pause_message: None,
            degraded_fields: summary.degraded_fields.clone(),
            revision: 0,
            elapsed_ms,
            received_at,
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        is_terminal_status(&self.status)
    }

    /// Active runs advance locally; terminal runs stay frozen at the server value.
    pub(crate) fn smoothed_elapsed_ms(&self, now: Instant) -> u64 {
        if self.is_terminal() {
            return self.elapsed_ms;
        }
        self.elapsed_ms
            .saturating_add(now.saturating_duration_since(self.received_at).as_millis() as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowCardChange {
    /// The card was created or replaced by a newer revision.
    Applied,
    /// The revision was not strictly newer than the last applied one.
    Duplicate,
    /// The run was cleared; the update was ignored.
    Cleared,
}

#[derive(Debug, Default)]
pub(crate) struct WorkflowCardStore {
    cards: BTreeMap<String, WorkflowCard>,
    revisions: BTreeMap<String, u64>,
    cleared: BTreeSet<String>,
}

impl WorkflowCardStore {
    pub(crate) fn apply_update(
        &mut self,
        notification: &WorkflowUpdatedNotification,
        now: Instant,
    ) -> WorkflowCardChange {
        if self.cleared.contains(&notification.run_id) {
            return WorkflowCardChange::Cleared;
        }
        if let Some(seen) = self.revisions.get(&notification.run_id)
            && notification.revision <= *seen
        {
            return WorkflowCardChange::Duplicate;
        }
        self.revisions
            .insert(notification.run_id.clone(), notification.revision);
        self.cards.insert(
            notification.run_id.clone(),
            WorkflowCard::from_notification(notification, now),
        );
        WorkflowCardChange::Applied
    }

    /// List responses fill gaps but never overwrite a richer live notification.
    pub(crate) fn apply_summaries(&mut self, runs: &[WorkflowRunSummary], now: Instant) {
        for summary in runs {
            if self.cleared.contains(&summary.run_id) || self.cards.contains_key(&summary.run_id) {
                continue;
            }
            self.cards.insert(
                summary.run_id.clone(),
                WorkflowCard::from_summary(summary, now),
            );
        }
    }

    /// Cleared tombstones come from persisted run metadata, not from
    /// `workflow/updated`; the transport for that marker is still pending.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "cleared markers are not yet delivered by workflow/updated"
        )
    )]
    pub(crate) fn clear(&mut self, run_id: &str) -> bool {
        self.cleared.insert(run_id.to_string());
        self.revisions.remove(run_id);
        self.cards.remove(run_id).is_some()
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "cleared markers are not yet delivered by workflow/updated"
        )
    )]
    pub(crate) fn is_cleared(&self, run_id: &str) -> bool {
        self.cleared.contains(run_id)
    }

    pub(crate) fn cards(&self) -> Vec<&WorkflowCard> {
        self.cards.values().collect()
    }

    pub(crate) fn card(&self, run_id: &str) -> Option<&WorkflowCard> {
        self.cards.get(run_id)
    }

    pub(crate) fn active_cards(&self) -> Vec<&WorkflowCard> {
        self.cards
            .values()
            .filter(|card| !card.is_terminal())
            .collect()
    }

    /// Used when a control command omits `run_id`.
    pub(crate) fn unique_active_run_id(&self) -> Option<&str> {
        let mut active = self.active_cards();
        if active.len() == 1 {
            active.pop().map(|card| card.run_id.as_str())
        } else {
            None
        }
    }
}

/// One-line-per-run rendering used by both transcript updates and `/workflows`.
pub(crate) fn card_text(card: &WorkflowCard, now: Instant) -> Vec<String> {
    let phase = card.phase.as_deref().unwrap_or("-");
    let budget = card
        .agent_budget
        .map(|budget| format!("/{budget}"))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "workflow {} {} [{}] phase={} agents={}{} elapsed={}s",
        card.run_id,
        card.name,
        card.status,
        phase,
        card.agents_used,
        budget,
        card.smoothed_elapsed_ms(now) / 1_000
    )];
    if let Some(agent) = &card.current_agent_label {
        lines.push(format!("  agent: {agent}"));
    }
    if let Some(event) = &card.last_event {
        let detail = card
            .last_event_detail
            .as_ref()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        lines.push(format!("  event: {event}{detail}"));
    }
    if let Some(message) = &card.pause_message {
        lines.push(format!("  paused: {message}"));
    }
    if let Some(summary) = &card.result_summary {
        lines.push(format!("  result: {summary}"));
    }
    if !card.degraded_fields.is_empty() {
        lines.push(format!("  degraded: {}", card.degraded_fields.join(", ")));
    }
    lines
}

pub(crate) fn dashboard_text(cards: &[&WorkflowCard], now: Instant) -> Vec<String> {
    if cards.is_empty() {
        return vec!["No workflow runs for this thread.".to_string()];
    }
    let mut lines = vec![format!("Workflow runs ({}):", cards.len())];
    for card in cards {
        lines.extend(
            card_text(card, now)
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }
    lines
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowHistoryCell {
    lines: Vec<Line<'static>>,
}

impl WorkflowHistoryCell {
    pub(crate) fn from_strings(lines: Vec<String>) -> Self {
        Self {
            lines: lines.into_iter().map(Line::from).collect(),
        }
    }

    pub(crate) fn from_card(card: &WorkflowCard, now: Instant) -> Self {
        Self::from_strings(card_text(card, now))
    }

    pub(crate) fn dashboard(cards: &[&WorkflowCard], now: Instant) -> Self {
        Self::from_strings(dashboard_text(cards, now))
    }
}

impl HistoryCell for WorkflowHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.lines.clone()
    }
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
