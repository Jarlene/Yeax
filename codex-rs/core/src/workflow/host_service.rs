//! Host side of the engine protocol (design 02 sections 4.1-4.3, TASK-028).
//!
//! The engine sends every host interaction through the 12-variant
//! [`WorkflowHostRequest`] channel. This service owns the run-scoped answers:
//! budget leases, phase/log/telemetry bookkeeping, scratch files, template
//! rendering, registry lookups, and git diffs. Agent spawning and interruption
//! stay behind [`AgentSpawner`] so this module is testable without a model or a
//! live child agent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use codex_workflow::MAX_AGENT_BUDGET;
use codex_workflow::host::AgentOpts;
use codex_workflow::host::AgentResult;
use codex_workflow::host::BudgetState;
use codex_workflow::host::HostError;
use codex_workflow::host::WorkflowHostRequest;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::assets;
use super::registry::WorkflowRegistry;
use super::store::Store;
use super::tracker::Tracker;

/// One child-agent spawn as seen by the host.
#[derive(Debug, Clone)]
pub(crate) struct SpawnRequest {
    pub run_id: String,
    pub phase: Option<String>,
    pub opts: AgentOpts,
}

pub(crate) type SpawnFuture =
    Pin<Box<dyn Future<Output = Result<AgentResult, HostError>> + Send + 'static>>;

/// The only piece of the host that needs `AgentControl` / `CodexThread`.
///
/// Keeping it behind a trait lets the host protocol be exercised with a fake
/// spawner; production uses [`super::runtime::Spawner`], which creates real
/// host children.
pub(crate) trait AgentSpawner: Send + Sync {
    fn resolve_workflow(&self, name: &str) -> Result<String, HostError>;
    fn spawn_agent(&self, request: SpawnRequest) -> SpawnFuture;
    fn git_diff_since(&self, commit: &str) -> Result<String, HostError>;
}

/// P0 agent options without a Yeax mapping must fail before a child is created.
///
/// Design 02 section 4.3 requires a visible `Unsupported("<field>")` instead of
/// silently dropping the value; `isolation_worktree: false` is the only
/// accepted spelling and is ignored.
fn unsupported_agent_field(opts: &AgentOpts) -> Option<&'static str> {
    if opts.max_output_tokens.is_some() {
        Some("max_output_tokens")
    } else if opts.capability_mode.is_some() {
        Some("capability_mode")
    } else if opts.isolation_worktree {
        Some("isolation_worktree")
    } else if opts.fork_context {
        Some("fork_context")
    } else if opts.resume_from.is_some() {
        Some("resume_from")
    } else {
        None
    }
}

/// Default in-tree implementation for the non-agent host verbs.
pub(crate) fn git_diff_since(cwd: &std::path::Path, commit: &str) -> Result<String, HostError> {
    if commit.is_empty() {
        return Err(HostError::Failed("empty commit".to_string()));
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("diff")
        .arg(commit)
        .output()
        .map_err(|error| HostError::Failed(format!("git diff: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(HostError::Failed(format!(
            "git diff {commit} failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) struct WorkflowHostService {
    run_id: String,
    tracker: Arc<Mutex<Tracker>>,
    store: Store,
    _registry: Arc<WorkflowRegistry>,
    spawner: Arc<dyn AgentSpawner>,
    cancel: CancellationToken,
    max_agent_calls: u64,
    admitted_agent_calls: Arc<Mutex<u64>>,
    spawn_slots: Arc<Semaphore>,
}

impl WorkflowHostService {
    pub(crate) fn new(
        run_id: String,
        tracker: Arc<Mutex<Tracker>>,
        store: Store,
        registry: Arc<WorkflowRegistry>,
        spawner: Arc<dyn AgentSpawner>,
        cancel: CancellationToken,
        max_concurrent_agents: u16,
    ) -> Self {
        Self {
            run_id,
            tracker,
            store,
            _registry: registry,
            spawner,
            cancel,
            max_agent_calls: MAX_AGENT_BUDGET * 2,
            admitted_agent_calls: Arc::new(Mutex::new(0)),
            spawn_slots: Arc::new(Semaphore::new(usize::from(max_concurrent_agents.max(1)))),
        }
    }

    /// Drain the host channel until the engine drops the sender.
    pub(crate) async fn serve(&self, mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>) {
        while let Some(request) = rx.recv().await {
            self.dispatch(request);
        }
    }

    pub(crate) fn dispatch(&self, request: WorkflowHostRequest) {
        match request {
            WorkflowHostRequest::ReserveAgentCalls { count, reply } => {
                let result = self.reserve_calls(count);
                let _ = reply.send(result);
            }
            WorkflowHostRequest::ReleaseAgentCalls { count, reply } => {
                let mut admitted = lock(&self.admitted_agent_calls);
                *admitted = admitted.saturating_sub(count);
                lock(&self.tracker).release_agents(count);
                let _ = reply.send(Ok(()));
            }
            WorkflowHostRequest::SpawnAgent { opts, reply } => {
                if self.cancel.is_cancelled() {
                    let _ = reply.send(Err(HostError::Cancelled));
                    return;
                }
                if let Some(field) = unsupported_agent_field(&opts) {
                    let _ = reply.send(Err(HostError::Unsupported(field.to_string())));
                    return;
                }
                let spawner = Arc::clone(&self.spawner);
                let slots = Arc::clone(&self.spawn_slots);
                let cancel = self.cancel.clone();
                let phase = opts.phase.clone();
                let run_id = self.run_id.clone();
                let tracker = Arc::clone(&self.tracker);
                tokio::spawn(async move {
                    let permit = match slots.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => {
                            let _ = reply.send(Err(HostError::Failed(
                                "workflow spawn gate closed".to_string(),
                            )));
                            return;
                        }
                    };
                    let label = opts
                        .label
                        .clone()
                        .or_else(|| opts.phase.clone())
                        .unwrap_or_else(|| "agent".to_string());
                    lock(&tracker).agent_started(label.clone(), label, phase, now_ms());
                    let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err(HostError::Cancelled),
                        result = spawner.spawn_agent(SpawnRequest {
                            run_id,
                            phase: opts.phase.clone(),
                            opts,
                        }) => result,
                    };
                    drop(permit);
                    let _ = reply.send(result);
                });
            }
            WorkflowHostRequest::ResolveWorkflow { name, reply } => {
                let result = self.spawner.resolve_workflow(&name);
                let _ = reply.send(result);
            }
            WorkflowHostRequest::Phase { title, .. } => {
                let _ = lock(&self.tracker).set_phase(title);
            }
            WorkflowHostRequest::Log { message, .. } => {
                let _ = lock(&self.tracker).push_event("log", message, None);
            }
            WorkflowHostRequest::Telemetry { name, fields, .. } => {
                let _ = lock(&self.tracker).push_event("telemetry", name, Some(fields.to_string()));
            }
            WorkflowHostRequest::BudgetQuery { reply } => {
                let snapshot = lock(&self.tracker).snapshot();
                let budget = snapshot.budget;
                let _ = reply.send(Ok(BudgetState {
                    total: budget.total,
                    spent: budget.spent,
                    reserved: budget.reserved,
                    remaining: budget.remaining,
                }));
            }
            WorkflowHostRequest::RenderTemplate { name, vars, reply } => {
                let result = assets::render_template(&name, &vars)
                    .map_err(|error| HostError::Failed(error.to_string()));
                let _ = reply.send(result);
            }
            WorkflowHostRequest::WriteScratchFile {
                name,
                content,
                reply,
            } => {
                let result = self
                    .store
                    .write_scratch(&self.run_id, &name, &content)
                    .map(|path| path.display().to_string())
                    .map_err(|error| HostError::Failed(error.to_string()));
                let _ = reply.send(result);
            }
            WorkflowHostRequest::ReadScratchFile { name, reply } => {
                let result = self
                    .store
                    .read_scratch(&self.run_id, &name)
                    .map_err(|error| HostError::Failed(error.to_string()));
                let _ = reply.send(result);
            }
            WorkflowHostRequest::GitDiffSince { commit, reply } => {
                let result = self.spawner.git_diff_since(&commit);
                let _ = reply.send(result);
            }
        }
    }

    /// Per-run logical agent-run cap: `MAX_AGENT_BUDGET * (retries + 1)`.
    fn reserve_calls(&self, count: u64) -> Result<(), HostError> {
        let mut admitted = lock(&self.admitted_agent_calls);
        let requested = admitted.saturating_add(count);
        if requested > self.max_agent_calls {
            return Err(HostError::AgentCallQuotaExceeded {
                requested,
                maximum: self.max_agent_calls,
            });
        }
        *admitted = requested;
        let _ = lock(&self.tracker).reserve_agents(count);
        Ok(())
    }

    /// Exposed so the run watcher can finish a run whose host channel closed.
    #[allow(dead_code)]
    pub(crate) fn close_spawn_gate(&self) {
        self.spawn_slots.close();
    }

    #[allow(dead_code)]
    pub(crate) fn spawn_capacity(&self) -> usize {
        self.spawn_slots.available_permits()
    }

    #[allow(dead_code)]
    pub(crate) fn registry(&self) -> &Arc<WorkflowRegistry> {
        &self._registry
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Drain timeout for the stop sequence (design 02 §5.5).
pub(crate) const STOP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
#[path = "host_service_tests.rs"]
mod tests;
