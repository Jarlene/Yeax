//! Queue in front of host_service `create/send` using the same capacity as
//! per-run admission. A turn may only start after the gate and the admission
//! permit have both been acquired.

use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::admission::capacity_for;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateError {
    /// The gate is full and the caller asked for a non-blocking entry.
    AgentLimitReached { max_threads: usize },
    /// The run was cancelled while queued.
    Cancelled,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentLimitReached { max_threads } => {
                write!(f, "workflow gate is full ({max_threads} concurrent turns)")
            }
            Self::Cancelled => write!(f, "workflow run was cancelled while queued"),
        }
    }
}

impl std::error::Error for GateError {}

#[derive(Debug)]
pub(crate) struct GatePermit {
    _permit: OwnedSemaphorePermit,
}

/// Same-capacity queue used before touching Core's host admission path.
#[derive(Debug)]
pub(crate) struct WorkflowRunGate {
    capacity: usize,
    semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl WorkflowRunGate {
    // Reserved convenience constructor; production builds the gate from the
    // already-clamped admission capacity with `with_capacity` (deviation D-11).
    #[allow(dead_code)]
    pub(crate) fn new(configured: Option<usize>, cancel: CancellationToken) -> Self {
        Self::with_capacity(capacity_for(configured), cancel)
    }

    /// Build a gate from an already-clamped admission capacity. Both layers
    /// must share the value; never recompute it independently.
    pub(crate) fn with_capacity(capacity: usize, cancel: CancellationToken) -> Self {
        Self {
            capacity,
            semaphore: Arc::new(Semaphore::new(capacity)),
            cancel,
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Non-blocking entry. Used when the caller already knows it must not wait.
    pub(crate) fn try_enter(&self) -> Result<GatePermit, GateError> {
        if self.cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map(|permit| GatePermit { _permit: permit })
            .map_err(|_| GateError::AgentLimitReached {
                max_threads: self.capacity,
            })
    }

    /// Wait for a slot. Cancellation is checked first and also wins the race
    /// against a newly freed permit, so a stopped run never starts a turn.
    pub(crate) async fn enter(&self) -> Result<GatePermit, GateError> {
        if self.cancel.is_cancelled() {
            return Err(GateError::Cancelled);
        }
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(GateError::Cancelled),
            permit = self.semaphore.clone().acquire_owned() => {
                match permit {
                    Ok(permit) => Ok(GatePermit { _permit: permit }),
                    Err(_) => Err(GateError::Cancelled),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
