//! Tests for the workflow history cell.

use super::WorkflowCard;
use super::WorkflowCardChange;
use super::WorkflowCardStore;
use super::WorkflowHistoryCell;
use super::card_text;
use super::dashboard_text;
use super::is_terminal_status;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::WorkflowAgentInfo;
use codex_app_server_protocol::WorkflowPhaseInfo;
use codex_app_server_protocol::WorkflowRunSummary;
use codex_app_server_protocol::WorkflowUpdatedNotification;
use std::time::Duration;
use std::time::Instant;

fn notification(run_id: &str, revision: u64, status: &str) -> WorkflowUpdatedNotification {
    WorkflowUpdatedNotification {
        thread_id: "thread-1".to_string(),
        run_id: run_id.to_string(),
        revision,
        name: "hello-workflow".to_string(),
        objective: "smoke".to_string(),
        status: status.to_string(),
        foreground: false,
        phases: vec![WorkflowPhaseInfo {
            title: "Research".to_string(),
            state: "active".to_string(),
        }],
        current_phase: Some("Research".to_string()),
        agent_budget: Some(8),
        agents_used: 2,
        agents_reserved: 1,
        agents_remaining: 5,
        agent_usage_incomplete: false,
        elapsed_ms: 1_500,
        active_agents: 1,
        current_agent_label: Some("researcher".to_string()),
        agents: vec![WorkflowAgentInfo {
            agent_id: "agent-1".to_string(),
            label: "researcher".to_string(),
            phase: Some("Research".to_string()),
            model: None,
            state: "running".to_string(),
            tokens_used: 10,
            duration_ms: 1_000,
        }],
        last_event: Some("agent_started".to_string()),
        last_event_detail: Some("researcher".to_string()),
        last_event_timestamp: Some("2026-11-23T00:00:00Z".to_string()),
        pause_message: None,
        result_summary: None,
        degraded_fields: vec!["agents".to_string()],
    }
}

fn summary(run_id: &str, status: &str) -> WorkflowRunSummary {
    WorkflowRunSummary {
        run_id: run_id.to_string(),
        name: "from-list".to_string(),
        status: status.to_string(),
        phase: Some("Plan".to_string()),
        started_at_ms: 1_000,
        updated_at_ms: 4_000,
        agent_count: 3,
        script_path: "/tmp/script.rhai".to_string(),
        run_dir: "/tmp/run".to_string(),
        degraded_fields: Vec::new(),
    }
}

#[test]
fn upsert_replaces_the_card_for_the_same_run_id() {
    let now = Instant::now();
    let mut store = WorkflowCardStore::default();
    assert_eq!(
        store.apply_update(&notification("run-1", 1, "active"), now),
        WorkflowCardChange::Applied
    );
    let mut second = notification("run-1", 2, "user_paused");
    second.elapsed_ms = 9_000;
    assert_eq!(
        store.apply_update(&second, now),
        WorkflowCardChange::Applied
    );
    let cards = store.cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].status, "user_paused");
    assert_eq!(cards[0].elapsed_ms, 9_000);
    assert_eq!(cards[0].revision, 2);
}

#[test]
fn stale_and_equal_revisions_are_ignored() {
    let now = Instant::now();
    let mut store = WorkflowCardStore::default();
    store.apply_update(&notification("run-1", 5, "active"), now);
    assert_eq!(
        store.apply_update(&notification("run-1", 5, "complete"), now),
        WorkflowCardChange::Duplicate
    );
    assert_eq!(
        store.apply_update(&notification("run-1", 4, "complete"), now),
        WorkflowCardChange::Duplicate
    );
    assert_eq!(store.cards()[0].status, "active");
}

#[test]
fn cleared_removes_the_card_and_tombstones_later_updates() {
    let now = Instant::now();
    let mut store = WorkflowCardStore::default();
    store.apply_update(&notification("run-1", 1, "active"), now);
    assert!(store.clear("run-1"));
    assert!(store.cards().is_empty());
    assert!(store.is_cleared("run-1"));
    assert_eq!(
        store.apply_update(&notification("run-1", 2, "active"), now),
        WorkflowCardChange::Cleared
    );
    // Clearing an unknown run still records the tombstone.
    assert!(!store.clear("run-2"));
    assert!(store.is_cleared("run-2"));
}

#[test]
fn list_summaries_fill_gaps_without_overwriting_live_cards() {
    let now = Instant::now();
    let mut store = WorkflowCardStore::default();
    store.apply_update(&notification("run-1", 3, "active"), now);
    store.apply_summaries(
        &[summary("run-1", "complete"), summary("run-2", "complete")],
        now,
    );
    let cards = store.cards();
    assert_eq!(cards.len(), 2);
    let live = cards.iter().find(|card| card.run_id == "run-1").unwrap();
    assert_eq!(live.status, "active");
    assert_eq!(live.revision, 3);
    let listed = cards.iter().find(|card| card.run_id == "run-2").unwrap();
    assert_eq!(listed.name, "from-list");
    assert_eq!(listed.elapsed_ms, 3_000);
}

#[test]
fn active_elapsed_is_smoothed_but_terminal_elapsed_is_frozen() {
    let received = Instant::now();
    let active = WorkflowCard::from_notification(&notification("run-1", 1, "active"), received);
    assert_eq!(
        active.smoothed_elapsed_ms(received + Duration::from_secs(2)),
        3_500
    );
    let terminal = WorkflowCard::from_notification(&notification("run-2", 2, "complete"), received);
    assert_eq!(
        terminal.smoothed_elapsed_ms(received + Duration::from_secs(2)),
        1_500
    );
    assert!(is_terminal_status("failed"));
    assert!(is_terminal_status("cancelled"));
    assert!(!is_terminal_status("blocked"));
}

#[test]
fn unique_active_run_id_requires_exactly_one_active_run() {
    let now = Instant::now();
    let mut store = WorkflowCardStore::default();
    assert_eq!(store.unique_active_run_id(), None);
    store.apply_update(&notification("run-1", 1, "active"), now);
    assert_eq!(store.unique_active_run_id(), Some("run-1"));
    store.apply_update(&notification("run-2", 1, "active"), now);
    assert_eq!(store.unique_active_run_id(), None);
    store.apply_update(&notification("run-1", 2, "complete"), now);
    assert_eq!(store.unique_active_run_id(), Some("run-2"));
}

#[test]
fn card_text_renders_status_phase_agents_and_optional_sections() {
    let now = Instant::now();
    let mut card = notification("run-1", 1, "active");
    card.pause_message = Some("waiting for permit".to_string());
    card.result_summary = Some("3/3 phases".to_string());
    let card = WorkflowCard::from_notification(&card, now);
    let text = card_text(&card, now).join("\n");
    assert!(text.contains("workflow run-1 hello-workflow [active]"));
    assert!(text.contains("phase=Research"));
    assert!(text.contains("agents=2/8"));
    assert!(text.contains("agent: researcher"));
    assert!(text.contains("event: agent_started (researcher)"));
    assert!(text.contains("paused: waiting for permit"));
    assert!(text.contains("result: 3/3 phases"));
    assert!(text.contains("degraded: agents"));
}

#[test]
fn dashboard_reports_an_empty_session_and_history_cell_exposes_the_lines() {
    let now = Instant::now();
    assert_eq!(
        dashboard_text(&[], now),
        vec!["No workflow runs for this thread.".to_string()]
    );
    let mut store = WorkflowCardStore::default();
    store.apply_update(&notification("run-1", 1, "active"), now);
    let cards = store.cards();
    let cell = WorkflowHistoryCell::dashboard(&cards, now);
    let rendered: Vec<String> = cell
        .display_lines(/*width*/ 120)
        .into_iter()
        .map(|line| line.to_string())
        .collect();
    assert_eq!(
        rendered,
        cell.raw_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert!(rendered[0].starts_with("Workflow runs (1):"));
    assert!(rendered[1].contains("workflow run-1"));
}
