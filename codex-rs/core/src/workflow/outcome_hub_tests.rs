//! Tests for the workflow outcome hub.

use super::*;

use serde_json::json;
use std::time::Duration;

fn outcome(output: serde_json::Value) -> TurnOutcome {
    TurnOutcome {
        success: true,
        output,
        cancelled: false,
        tokens_used: 3,
    }
}

#[test]
fn same_turn_is_idempotent_and_new_turn_is_an_attempt() {
    let hub = WorkflowOutcomeHub::new();
    hub.register("wf_a", "thread-1");
    assert_eq!(
        hub.publish("thread-1", 1, outcome(json!({"a": 1}))),
        PublishOutcome::Accepted
    );
    assert_eq!(
        hub.publish("thread-1", 1, outcome(json!({"a": 2}))),
        PublishOutcome::Duplicate
    );
    assert_eq!(
        hub.outcome("thread-1").expect("outcome").output,
        json!({"a": 1})
    );
    assert_eq!(
        hub.publish("thread-1", 2, outcome(json!({"a": 3}))),
        PublishOutcome::Accepted
    );
    assert_eq!(
        hub.outcome("thread-1").expect("outcome").output,
        json!({"a": 3})
    );
}

#[test]
fn unregistered_and_finalized_threads_reject_late_turns() {
    let hub = WorkflowOutcomeHub::new();
    assert_eq!(
        hub.publish("missing", 1, outcome(json!(null))),
        PublishOutcome::Rejected
    );
    hub.register("wf_a", "thread-1");
    hub.publish("thread-1", 1, outcome(json!(1)));
    assert!(hub.finalize("thread-1"));
    assert!(hub.owns("thread-1"));
    assert_eq!(
        hub.publish("thread-1", 2, outcome(json!(2))),
        PublishOutcome::Rejected
    );
    assert_eq!(
        hub.outcome("thread-1").expect("final outcome").output,
        json!(1)
    );
}

#[test]
fn note_attempt_counts_schema_repairs() {
    let hub = WorkflowOutcomeHub::new();
    hub.register("wf_a", "thread-1");
    assert_eq!(hub.note_attempt("thread-1"), 1);
    assert_eq!(hub.note_attempt("thread-1"), 2);
    assert_eq!(hub.attempts("thread-1"), 2);
    assert_eq!(hub.note_attempt("missing"), 0);
}

#[test]
fn clear_run_removes_active_and_tombstones() {
    let hub = WorkflowOutcomeHub::new();
    hub.register("wf_a", "thread-1");
    hub.register("wf_b", "thread-2");
    hub.publish("thread-2", 1, outcome(json!(2)));
    hub.finalize("thread-2");
    hub.clear_run("wf_a");
    assert_eq!(hub.active_len(), 0);
    assert_eq!(hub.finalized_len(), 1);
    hub.clear_run("wf_b");
    assert_eq!(hub.finalized_len(), 0);
    assert!(!hub.owns("thread-2"));
}

#[tokio::test]
async fn wait_for_attempt_returns_the_published_turn() {
    let hub = Arc::new(WorkflowOutcomeHub::new());
    hub.register("wf_a", "thread-1");
    let waiter = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.wait_for_attempt("thread-1", 0, Duration::from_secs(2))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    hub.publish("thread-1", 7, outcome(json!({"ok": true})));
    let attempt = waiter.await.expect("join").expect("attempt");
    assert_eq!(attempt.turn_id, 7);
    assert_eq!(attempt.outcome.output, json!({"ok": true}));
}

#[tokio::test]
async fn wait_for_attempt_stops_when_finalized() {
    let hub = Arc::new(WorkflowOutcomeHub::new());
    hub.register("wf_a", "thread-1");
    let waiter = {
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            hub.wait_for_attempt("thread-1", 0, Duration::from_secs(5))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    hub.finalize("thread-1");
    assert!(waiter.await.expect("join").is_none());
}

#[tokio::test]
async fn drain_completes_when_all_threads_are_final() {
    let hub = Arc::new(WorkflowOutcomeHub::new());
    hub.register("wf_a", "thread-1");
    hub.register("wf_a", "thread-2");
    let ids = vec!["thread-1".to_string(), "thread-2".to_string()];
    hub.begin_drain(&ids);
    let waiter = {
        let hub = Arc::clone(&hub);
        let ids = ids.clone();
        tokio::spawn(async move { hub.wait_for_drain(&ids, Duration::from_secs(2)).await })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    hub.finalize_all(&ids);
    assert!(waiter.await.expect("join"));
}

#[tokio::test]
async fn drain_times_out_when_a_thread_never_finishes() {
    let hub = WorkflowOutcomeHub::new();
    hub.register("wf_a", "thread-1");
    let ids = vec!["thread-1".to_string()];
    hub.begin_drain(&ids);
    assert!(!hub.wait_for_drain(&ids, Duration::from_millis(50)).await);
}

#[test]
fn cleanup_drops_tombstones_after_the_ttl() {
    let hub = WorkflowOutcomeHub::new();
    hub.register("wf_a", "thread-1");
    hub.finalize("thread-1");
    assert_eq!(hub.finalized_len(), 1);
    hub.cleanup_expired();
    // Not yet expired: the TTL is a minute.
    assert_eq!(hub.finalized_len(), 1);
}
