//! Tests for workflow run admission control.

use super::*;

fn cpus() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(MIN_WORKFLOW_AGENT_CAPACITY)
        .max(MIN_WORKFLOW_AGENT_CAPACITY)
}

#[test]
fn capacity_formula_matches_design() {
    assert_eq!(capacity_for(None), 32.min(cpus()));
    assert_eq!(capacity_for(Some(0)), 1);
    assert_eq!(capacity_for(Some(1)), 1);
    let requested = cpus() + 100;
    assert_eq!(capacity_for(Some(requested)), cpus());
    if cpus() >= 3 {
        assert_eq!(capacity_for(Some(3)), 3);
    }
}

#[test]
fn n_permits_succeed_and_n_plus_one_is_rejected() {
    let admission = WorkflowRunAdmission::new(Some(2), CancellationToken::new());
    assert_eq!(admission.capacity(), 2);
    let first = admission.try_admit().expect("first");
    let second = admission.try_admit().expect("second");
    assert_eq!(admission.available(), 0);
    let error = admission.try_admit().expect_err("third");
    assert_eq!(error, AdmissionError::AgentLimitReached { max_threads: 2 });
    drop(first);
    assert_eq!(admission.available(), 1);
    let third = admission.try_admit().expect("reused slot");
    drop((second, third));
    assert_eq!(admission.available(), 2);
}

#[test]
fn cancellation_wins_over_a_free_slot() {
    let cancel = CancellationToken::new();
    let admission = WorkflowRunAdmission::new(Some(2), cancel.clone());
    assert_eq!(admission.available(), 2);
    cancel.cancel();
    assert_eq!(
        admission.try_admit().unwrap_err(),
        AdmissionError::Cancelled
    );
}

#[test]
fn concurrent_competition_never_exceeds_capacity() {
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let admission = Arc::new(WorkflowRunAdmission::new(Some(3), CancellationToken::new()));
    let barrier = Arc::new(Barrier::new(9));
    let held = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let admission = Arc::clone(&admission);
        let barrier = Arc::clone(&barrier);
        let held = Arc::clone(&held);
        let peak = Arc::clone(&peak);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            match admission.try_admit() {
                Ok(permit) => {
                    let now = held.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    held.fetch_sub(1, Ordering::SeqCst);
                    drop(permit);
                    true
                }
                Err(_) => false,
            }
        }));
    }
    barrier.wait();
    let granted = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .filter(|granted| *granted)
        .count();
    assert_eq!(granted, 3);
    assert!(peak.load(Ordering::SeqCst) <= 3);
    assert_eq!(admission.available(), 3);
}
