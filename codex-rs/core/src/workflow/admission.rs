//! Per-run turn admission for workflow child agents.
//!
//! Capacity is clamped once: `configured.unwrap_or(32).max(1).min(cpus.max(2))`.
//! The `AgentTurnAdmission` implementation only ever exposes a per-turn permit;
//! the capacity value itself stays private to this module so the gate and the
//! admission layer cannot drift apart.
//!
//! Host integration is compiled only into the real `codex-core` crate. The
//! standalone logic harness sets `--cfg standalone_logic` to exercise the
//! capacity/permit behavior without the rest of Core.

use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub(crate) const DEFAULT_WORKFLOW_AGENT_CAPACITY: usize = 32;
pub(crate) const MIN_WORKFLOW_AGENT_CAPACITY: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    /// No permit is currently free. Core maps this to
    /// `AgentLimitReached { max_threads }` without waiting.
    AgentLimitReached { max_threads: usize },
    /// The run was cancelled before the permit could be handed out.
    Cancelled,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentLimitReached { max_threads } => {
                write!(
                    f,
                    "workflow agent limit reached ({max_threads} concurrent turns)"
                )
            }
            Self::Cancelled => write!(f, "workflow run was cancelled"),
        }
    }
}

impl std::error::Error for AdmissionError {}

/// Effective per-run capacity. `available_parallelism` failing falls back to
/// the documented minimum of 2.
pub(crate) fn capacity_for(configured: Option<usize>) -> usize {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(MIN_WORKFLOW_AGENT_CAPACITY)
        .max(MIN_WORKFLOW_AGENT_CAPACITY);
    configured
        .unwrap_or(DEFAULT_WORKFLOW_AGENT_CAPACITY)
        .max(1)
        .min(cpus)
}

/// A permit that returns its slot when dropped.
#[derive(Debug)]
pub(crate) struct AdmissionPermit {
    _permit: OwnedSemaphorePermit,
}

/// Per-run `AgentTurnAdmission` provider.
#[derive(Debug)]
pub(crate) struct WorkflowRunAdmission {
    capacity: usize,
    semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl WorkflowRunAdmission {
    pub(crate) fn new(configured: Option<usize>, cancel: CancellationToken) -> Self {
        let capacity = capacity_for(configured);
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

    pub(crate) fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Try to reserve one turn slot without waiting. Cancellation wins over an
    /// otherwise free slot so a cancelled run never starts a new turn.
    pub(crate) fn try_admit(&self) -> Result<AdmissionPermit, AdmissionError> {
        if self.cancel.is_cancelled() {
            return Err(AdmissionError::Cancelled);
        }
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map(|permit| AdmissionPermit { _permit: permit })
            .map_err(|_| AdmissionError::AgentLimitReached {
                max_threads: self.capacity,
            })
    }
}

#[allow(unexpected_cfgs)]
#[cfg(not(standalone_logic))]
mod host {
    use futures::FutureExt;
    use futures::future::BoxFuture;

    use crate::agent::host::AgentTurnAdmission;
    use crate::agent::host::AgentTurnAdmissionRequest;
    use crate::agent::types::AgentExecutionGuard;
    use crate::workflow::admission::AdmissionError;
    use crate::workflow::admission::WorkflowRunAdmission;
    use codex_protocol::error::CodexErr;
    use codex_protocol::error::CodexErrorDetails;
    use codex_protocol::error::Result;

    impl AgentTurnAdmission for WorkflowRunAdmission {
        fn admit_turn(
            &self,
            _request: AgentTurnAdmissionRequest,
        ) -> BoxFuture<'_, Result<AgentExecutionGuard>> {
            async move {
                let permit = self.try_admit().map_err(|error| match error {
                    AdmissionError::AgentLimitReached { max_threads } => {
                        CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads })
                    }
                    AdmissionError::Cancelled => {
                        CodexErr::InvalidRequest("workflow run was cancelled".to_string())
                    }
                })?;
                Ok(AgentExecutionGuard::new(permit))
            }
            .boxed()
        }
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
