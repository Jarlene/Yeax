//! Tests for workflow run gating.

use std::time::Duration;

use super::super::admission::WorkflowRunAdmission;
use super::*;

#[tokio::test]
async fn gate_queues_n_then_rejects_n_plus_one_without_waiting() {
    let gate = WorkflowRunGate::new(Some(2), CancellationToken::new());
    assert_eq!(gate.capacity(), 2);
    let first = gate.try_enter().expect("first");
    let second = gate.try_enter().expect("second");
    assert_eq!(
        gate.try_enter().unwrap_err(),
        GateError::AgentLimitReached { max_threads: 2 }
    );
    drop(first);
    drop(second);
    assert_eq!(gate.available(), 2);
}

#[tokio::test]
async fn enter_waits_for_a_freed_slot() {
    let gate = Arc::new(WorkflowRunGate::new(Some(1), CancellationToken::new()));
    let held = gate.enter().await.expect("first");
    let waiting = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { gate.enter().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiting.is_finished());
    drop(held);
    let permit = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("wait timed out")
        .expect("join")
        .expect("permit");
    drop(permit);
    assert_eq!(gate.available(), 1);
}

#[tokio::test]
async fn cancellation_fails_a_queued_entry() {
    let cancel = CancellationToken::new();
    let gate = Arc::new(WorkflowRunGate::new(Some(1), cancel.clone()));
    let held = gate.enter().await.expect("first");
    let waiting = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move { gate.enter().await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("wait timed out")
        .expect("join")
        .expect_err("cancelled");
    assert_eq!(error, GateError::Cancelled);
    drop(held);
}

#[test]
fn gate_and_admission_share_one_capacity() {
    let admission = WorkflowRunAdmission::new(Some(5), CancellationToken::new());
    let gate = WorkflowRunGate::with_capacity(admission.capacity(), CancellationToken::new());
    assert_eq!(gate.capacity(), admission.capacity());
    let default_admission = WorkflowRunAdmission::new(None, CancellationToken::new());
    let default_gate = WorkflowRunGate::new(None, CancellationToken::new());
    assert_eq!(default_gate.capacity(), default_admission.capacity());
}
