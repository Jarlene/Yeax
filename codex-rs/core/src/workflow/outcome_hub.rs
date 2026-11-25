//! Terminal-outcome collection for workflow child agents.
//!
//! Design 02 section 5.3. Every child agent gets a reporter bound to
//! `(run_id, thread_id)`. A thread may produce one terminal turn, or several
//! turns when the schema contract asks for a repair attempt; only the last
//! attempt is finalized. Finalized threads keep a tombstone so a late terminal
//! turn cannot resurrect a run that already moved on.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::agent::control::WorkflowChildRegistry;

/// Tombstones suppress late terminal turns for one minute (design 02 §5.5).
pub(crate) const FINALIZED_TOMBSTONE_TTL: Duration = Duration::from_secs(60);

/// Minimal terminal view of one agent turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnOutcome {
    pub success: bool,
    pub output: Value,
    pub cancelled: bool,
    pub tokens_used: u64,
}

/// Result of `publish`, used by callers to log late terminals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    /// A new attempt became the current result.
    Accepted,
    /// The same `(thread_id, turn_id)` was already published.
    Duplicate,
    /// The thread was finalized (tombstone) or never registered.
    Rejected,
}

/// The attempt a host service waits for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Attempt {
    pub seq: u64,
    pub turn_id: u64,
    pub outcome: TurnOutcome,
}

struct ActiveEntry {
    run_id: String,
    turn_id: Option<u64>,
    attempt_seq: u64,
    attempts: u32,
    outcome: Option<TurnOutcome>,
    wake: Arc<Notify>,
}

struct FinalizedEntry {
    run_id: String,
    outcome: Option<TurnOutcome>,
    finalized_at: Instant,
}

#[derive(Default)]
struct HubInner {
    active: HashMap<String, ActiveEntry>,
    finalized: HashMap<String, FinalizedEntry>,
    draining: Vec<String>,
}

/// Session-scoped hub shared by `host_service` and the spawn reporters.
#[derive(Default)]
pub(crate) struct WorkflowOutcomeHub {
    inner: Mutex<HubInner>,
    /// Optional tree registry kept in step with finalization and cleanup.
    registry: Option<Arc<WorkflowChildRegistry>>,
}

impl WorkflowOutcomeHub {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Production constructor: finalization also moves the tree's registration
    /// to a tombstone so late terminals stay suppressed.
    pub(crate) fn with_registry(registry: Arc<WorkflowChildRegistry>) -> Self {
        Self {
            inner: Mutex::default(),
            registry: Some(registry),
        }
    }

    /// Register a child thread before its first turn is admitted.
    pub(crate) fn register(&self, run_id: &str, thread_id: &str) {
        self.cleanup_expired();
        let mut inner = lock(&self.inner);
        if inner.finalized.contains_key(thread_id) {
            return;
        }
        inner
            .active
            .entry(thread_id.to_string())
            .or_insert_with(|| ActiveEntry {
                run_id: run_id.to_string(),
                turn_id: None,
                attempt_seq: 0,
                attempts: 0,
                outcome: None,
                wake: Arc::new(Notify::new()),
            });
    }

    /// Publish one terminal turn. Same turn is idempotent; a new turn becomes
    /// the current attempt.
    pub(crate) fn publish(
        &self,
        thread_id: &str,
        turn_id: u64,
        outcome: TurnOutcome,
    ) -> PublishOutcome {
        let mut inner = lock(&self.inner);
        if inner.finalized.contains_key(thread_id) {
            return PublishOutcome::Rejected;
        }
        let Some(entry) = inner.active.get_mut(thread_id) else {
            return PublishOutcome::Rejected;
        };
        if entry.turn_id == Some(turn_id) {
            return PublishOutcome::Duplicate;
        }
        entry.turn_id = Some(turn_id);
        entry.attempt_seq += 1;
        entry.attempts = entry.attempts.max(entry.attempt_seq as u32);
        entry.outcome = Some(outcome);
        entry.wake.notify_waiters();
        PublishOutcome::Accepted
    }

    /// Schema retry: bump the attempt counter so the next publish is treated as
    /// a repair attempt, not a duplicate.
    pub(crate) fn note_attempt(&self, thread_id: &str) -> u32 {
        let mut inner = lock(&self.inner);
        if let Some(registry) = &self.registry
            && let Ok(thread_id) = codex_protocol::ThreadId::from_string(thread_id)
        {
            registry.note_attempt(thread_id);
        }
        let Some(entry) = inner.active.get_mut(thread_id) else {
            return 0;
        };
        entry.attempts += 1;
        entry.attempts
    }

    pub(crate) fn attempts(&self, thread_id: &str) -> u32 {
        lock(&self.inner)
            .active
            .get(thread_id)
            .map(|entry| entry.attempts)
            .unwrap_or(0)
    }

    /// Mark the current attempt final and move the entry to the tombstone map.
    pub(crate) fn finalize(&self, thread_id: &str) -> bool {
        let attempts = self
            .registry
            .as_ref()
            .and_then(|registry| {
                codex_protocol::ThreadId::from_string(thread_id)
                    .ok()
                    .and_then(|id| registry.attempts(id))
            })
            .unwrap_or(0);
        if let Some(registry) = &self.registry
            && let Ok(thread_id) = codex_protocol::ThreadId::from_string(thread_id)
        {
            registry.finalize(thread_id);
        }
        let mut inner = lock(&self.inner);
        let Some(entry) = inner.active.remove(thread_id) else {
            return false;
        };
        entry.wake.notify_waiters();
        let has_outcome = entry.outcome.is_some();
        inner.finalized.insert(
            thread_id.to_string(),
            FinalizedEntry {
                run_id: entry.run_id,
                outcome: entry.outcome,
                finalized_at: Instant::now(),
            },
        );
        tracing::debug!(thread_id, has_outcome, attempts, "workflow child finalized");
        true
    }

    /// Active or tombstoned threads both count as owned, which suppresses late
    /// terminals without clearing the record early.
    #[cfg(test)]
    pub(crate) fn owns(&self, thread_id: &str) -> bool {
        let inner = lock(&self.inner);
        inner.active.contains_key(thread_id) || inner.finalized.contains_key(thread_id)
    }

    pub(crate) fn outcome(&self, thread_id: &str) -> Option<TurnOutcome> {
        let inner = lock(&self.inner);
        inner
            .active
            .get(thread_id)
            .and_then(|entry| entry.outcome.clone())
            .or_else(|| {
                inner
                    .finalized
                    .get(thread_id)
                    .and_then(|entry| entry.outcome.clone())
            })
    }

    /// Wait until an attempt newer than `after_seq` arrives, or the thread is
    /// finalized. Returns `None` on finalize, timeout, or unknown thread.
    pub(crate) async fn wait_for_attempt(
        &self,
        thread_id: &str,
        after_seq: u64,
        limit: Duration,
    ) -> Option<Attempt> {
        let deadline = Instant::now() + limit;
        loop {
            let (wake, current) = {
                let inner = lock(&self.inner);
                let entry = inner.active.get(thread_id)?;
                let current = if entry.attempt_seq > after_seq {
                    entry.turn_id.map(|turn_id| Attempt {
                        seq: entry.attempt_seq,
                        turn_id,
                        outcome: entry.outcome.clone().unwrap_or(TurnOutcome {
                            success: false,
                            output: Value::Null,
                            cancelled: false,
                            tokens_used: 0,
                        }),
                    })
                } else {
                    None
                };
                (Arc::clone(&entry.wake), current)
            };
            if let Some(attempt) = current {
                return Some(attempt);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let notified = wake.notified();
            // Re-check after creating the waiter so a publish that raced the
            // previous lock acquisition is not lost.
            {
                let inner = lock(&self.inner);
                match inner.active.get(thread_id) {
                    None => return None,
                    Some(entry) if entry.attempt_seq > after_seq => {
                        let turn_id = entry.turn_id?;
                        return Some(Attempt {
                            seq: entry.attempt_seq,
                            turn_id,
                            outcome: entry.outcome.clone().unwrap_or(TurnOutcome {
                                success: false,
                                output: Value::Null,
                                cancelled: false,
                                tokens_used: 0,
                            }),
                        });
                    }
                    Some(_) => {}
                }
            }
            if timeout(deadline - now, notified).await.is_err() {
                return None;
            }
        }
    }

    /// Step 4/5 of the stop sequence: wait for every thread to reach a terminal
    /// state. Idle children already finalized count immediately.
    pub(crate) fn begin_drain(&self, thread_ids: &[String]) {
        let mut inner = lock(&self.inner);
        inner.draining = thread_ids.to_vec();
    }

    /// Returns true when every requested thread is finalized before the limit.
    pub(crate) async fn wait_for_drain(&self, thread_ids: &[String], limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            let (pending, wake) = {
                let inner = lock(&self.inner);
                let pending: Vec<String> = thread_ids
                    .iter()
                    .filter(|id| !inner.finalized.contains_key(*id))
                    .cloned()
                    .collect();
                let wake = inner
                    .active
                    .values()
                    .next()
                    .map(|entry| Arc::clone(&entry.wake));
                (pending, wake)
            };
            if pending.is_empty() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            match wake {
                Some(wake) => {
                    let notified = wake.notified();
                    if timeout(deadline - now, notified).await.is_err() {
                        return false;
                    }
                }
                None => {
                    // No active entry can ever wake us: wait for the deadline.
                    tokio::time::sleep(deadline - now).await;
                    return false;
                }
            }
        }
    }

    pub(crate) fn finalize_all(&self, thread_ids: &[String]) {
        for thread_id in thread_ids {
            self.finalize(thread_id);
        }
        lock(&self.inner).draining.clear();
    }

    /// Clear both active rows and tombstones for one run.
    pub(crate) fn clear_run(&self, run_id: &str) {
        let active = self.active_len();
        let finalized = self.finalized_len();
        tracing::debug!(
            run_id,
            active,
            finalized,
            "clearing workflow outcome hub state"
        );
        if let Some(registry) = &self.registry {
            registry.clear_run(run_id);
        }
        let mut inner = lock(&self.inner);
        inner.active.retain(|_, entry| entry.run_id != run_id);
        inner.finalized.retain(|_, entry| entry.run_id != run_id);
        inner.draining.clear();
    }

    /// Drop tombstones older than the TTL (late terminals are still rejected
    /// because by then the run record is terminal).
    pub(crate) fn cleanup_expired(&self) {
        let now = Instant::now();
        lock(&self.inner)
            .finalized
            .retain(|_, entry| now.duration_since(entry.finalized_at) < FINALIZED_TOMBSTONE_TTL);
    }

    pub(crate) fn active_len(&self) -> usize {
        lock(&self.inner).active.len()
    }

    pub(crate) fn finalized_len(&self) -> usize {
        lock(&self.inner).finalized.len()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "outcome_hub_tests.rs"]
mod tests;
