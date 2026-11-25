//! Lifecycle orchestrator for background workflow runs.
//!
//! The manager owns run records, the active-run budget, pause/resume/stop
//! intent, and terminal persistence. Engine execution and agent spawning stay
//! behind [`WorkflowRunRunner`] so this module has no dependency on
//! `AgentControl`, `CodexThread`, or tool invocation state.

// The lifecycle manager intentionally exposes the full resume/save/snapshot
// surface required by TASK-030; the current product path calls a subset and the
// remainder is exercised by `run_manager_tests`. Deviation D-12 records this.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_workflow::run::{PauseKind, WorkflowOutcome};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::manager::TaskNames;
use super::manager::WorkflowControlError;
use super::manager::WorkflowLimits;
use super::paths::WorkflowPaths;
use super::registry;
use super::registry::ResolvedWorkflow;
use super::registry::WorkflowOrigin;
use super::registry::WorkflowRegistry;
use super::request_service::ControlAction;
use super::request_service::RunScriptLookup;
use super::store::RunState;
use super::store::Store;
use super::tracker::AgentPhase;
use super::tracker::PauseReason;
use super::tracker::RunStatus;
use super::tracker::Tracker;
use super::tracker::WorkflowSnapshot;

/// Inputs already resolved and validated by `RequestService`.
pub(crate) struct LaunchRequest {
    pub enabled: bool,
    pub name: String,
    pub objective: String,
    pub script: String,
    pub script_path: Option<PathBuf>,
    pub args: Value,
    pub effort: Option<String>,
    pub agent_budget: Option<u64>,
    pub phases: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct LaunchOutcome {
    pub run_id: String,
    pub task_id: String,
    pub name: String,
    pub display_name: String,
    pub script_path: String,
    pub message: String,
}

/// Read-only run view shared with app-server (`TASK-033`) and the TUI.
///
/// Field order is fixed so the protocol DTO can map it one-to-one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunSummary {
    pub run_id: String,
    pub name: String,
    pub status: String,
    pub phase: Option<String>,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub agent_count: u32,
    pub script_path: PathBuf,
    pub run_dir: PathBuf,
    pub degraded_fields: Vec<String>,
}

/// Everything a runner needs to execute one script. The runner owns host I/O.
#[derive(Clone)]
pub(crate) struct RunContext {
    pub run_id: String,
    pub name: String,
    pub script: String,
    pub args: Value,
    pub effort: Option<String>,
    pub agent_budget: Option<u64>,
    pub paths: Arc<WorkflowPaths>,
    pub max_concurrent_agents: u16,
    pub script_path: PathBuf,
    pub args_path: PathBuf,
    pub effort_path: PathBuf,
    pub scratch_dir: PathBuf,
    pub cancel: CancellationToken,
    pub tracker: Arc<Mutex<Tracker>>,
    pub execution_epoch: u64,
}

pub(crate) type RunFuture = Pin<Box<dyn Future<Output = WorkflowOutcome> + Send + 'static>>;

/// Executes the engine plus host service. Injected so lifecycle tests do not
/// need a model, a runtime, or the filesystem beyond the run record itself.
pub(crate) trait WorkflowRunRunner: Send + Sync {
    fn start(&self, context: RunContext) -> RunFuture;
}

struct RunHandle {
    run_id: String,
    display_name: String,
    script_path: PathBuf,
    args: Value,
    cancel: CancellationToken,
    tracker: Arc<Mutex<Tracker>>,
    names: Mutex<TaskNames>,
    execution_epoch: AtomicU64,
}

impl RunHandle {
    fn snapshot(&self) -> WorkflowSnapshot {
        lock(&self.tracker).snapshot()
    }

    fn status(&self) -> RunStatus {
        lock(&self.tracker).status()
    }
}

pub struct WorkflowManager {
    paths: Arc<WorkflowPaths>,
    store: Store,
    runner: Arc<dyn WorkflowRunRunner>,
    limits: WorkflowLimits,
    runs: Mutex<HashMap<String, Arc<RunHandle>>>,
    events: broadcast::Sender<WorkflowSnapshot>,
    next_seq: AtomicU64,
}

impl WorkflowManager {
    pub(crate) fn new(paths: Arc<WorkflowPaths>, runner: Arc<dyn WorkflowRunRunner>) -> Self {
        Self::with_limits(paths, runner, WorkflowLimits::default())
    }

    /// Build a manager whose capacity and budgets come from `[workflow]`.
    pub(crate) fn with_limits(
        paths: Arc<WorkflowPaths>,
        runner: Arc<dyn WorkflowRunRunner>,
        limits: WorkflowLimits,
    ) -> Self {
        let store = Store::new(paths.session_root().to_path_buf());
        let (events, _) = broadcast::channel(64);
        Self {
            paths,
            store,
            runner,
            limits,
            runs: Mutex::new(HashMap::new()),
            events,
            next_seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn limits(&self) -> WorkflowLimits {
        self.limits
    }

    pub(crate) fn paths(&self) -> &Arc<WorkflowPaths> {
        &self.paths
    }

    /// Four-scope registry rooted at the same paths as the manager.
    pub(crate) fn registry(&self) -> Result<WorkflowRegistry, WorkflowControlError> {
        WorkflowRegistry::new(Arc::clone(&self.paths))
            .map_err(|error| WorkflowControlError::Unavailable(error.to_string()))
    }

    /// Launch the result of `RequestService::prepare`, so the tool handler has a
    /// single owner of registry resolution and feature gating.
    pub(crate) fn launch_prepared(
        &self,
        workflow: ResolvedWorkflow,
        args: Value,
        agent_budget: Option<u64>,
        enabled: bool,
    ) -> Result<LaunchOutcome, WorkflowControlError> {
        let name = workflow.meta.name.clone();
        if let Some(scope) = workflow.scope {
            tracing::debug!(
                workflow = %name,
                scope = scope.label(),
                "launching resolved workflow"
            );
        }
        let objective = args
            .get("objective")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| workflow.meta.description.clone());
        let phases = workflow
            .meta
            .phases
            .iter()
            .map(|phase| phase.title.clone())
            .collect();
        let script_path = match &workflow.origin {
            WorkflowOrigin::File(path) => Some(path.clone()),
            WorkflowOrigin::Builtin | WorkflowOrigin::Inline => None,
        };
        self.launch(LaunchRequest {
            enabled,
            name,
            objective,
            script: workflow.script,
            script_path,
            args,
            effort: None,
            agent_budget,
            phases,
        })
    }

    /// Pause or stop a run resolved by `RequestService`.
    pub(crate) fn control(
        &self,
        action: ControlAction,
        run_id: &str,
    ) -> Result<WorkflowSnapshot, WorkflowControlError> {
        match action {
            ControlAction::Pause => self.pause(run_id, PauseKind::User, "paused by user"),
            ControlAction::Stop => self.stop(run_id),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<WorkflowSnapshot> {
        self.events.subscribe()
    }

    pub(crate) fn active_count(&self) -> usize {
        // "Active" is the literal `active` status from design 03 section 4.5;
        // paused and budget-limited runs keep their records but no longer
        // consume one of the four launch slots.
        lock(&self.runs)
            .values()
            .filter(|handle| handle.status() == RunStatus::Active)
            .count()
    }

    /// Design 03 section 5.2: reject before creating any run record.
    pub(crate) fn launch(
        &self,
        request: LaunchRequest,
    ) -> Result<LaunchOutcome, WorkflowControlError> {
        if !request.enabled {
            return Err(WorkflowControlError::Disabled);
        }
        let active = self.active_count();
        if active >= self.limits.max_active_runs {
            return Err(WorkflowControlError::MaxActiveRunsExceeded {
                active,
                max: self.limits.max_active_runs,
            });
        }
        let agent_budget = request
            .agent_budget
            .or(Some(self.limits.default_agent_budget));
        let run_id = self.next_run_id();
        let args_json = serde_json::to_string(&request.args)
            .map_err(|error| WorkflowControlError::LaunchFailed(error.to_string()))?;
        let now = now_ms();
        let layout = self
            .store
            .create_run(
                &run_id,
                &request.name,
                &request.objective,
                &request.script,
                &args_json,
                request.effort.as_deref(),
                now,
            )
            .map_err(|error| WorkflowControlError::LaunchFailed(error.to_string()))?;
        let display_name = self
            .store
            .load_manifest()
            .ok()
            .and_then(|load| {
                load.manifest
                    .runs
                    .iter()
                    .find(|entry| entry.run_id == run_id)
                    .map(|entry| entry.display_name.clone())
            })
            .unwrap_or_else(|| request.name.clone());
        let tracker = Arc::new(Mutex::new(Tracker::new(
            run_id.clone(),
            request.name.clone(),
            request.objective.clone(),
            request.phases.clone(),
            agent_budget,
            true,
            now,
        )));
        let cancel = CancellationToken::new();
        let handle = Arc::new(RunHandle {
            run_id: run_id.clone(),
            display_name: display_name.clone(),
            script_path: layout.script_path(),
            args: request.args.clone(),
            cancel: cancel.clone(),
            tracker: Arc::clone(&tracker),
            names: Mutex::new(TaskNames::default()),
            execution_epoch: AtomicU64::new(1),
        });
        lock(&self.runs).insert(run_id.clone(), Arc::clone(&handle));
        self.persist_state(&handle, now);

        let context = RunContext {
            run_id: run_id.clone(),
            name: request.name.clone(),
            script: request.script,
            args: request.args,
            effort: request.effort,
            agent_budget,
            paths: Arc::clone(&self.paths),
            max_concurrent_agents: self.limits.max_concurrent_agents,
            script_path: handle.script_path.clone(),
            args_path: layout.args_path(),
            effort_path: layout.effort_path(),
            scratch_dir: layout.scratch_dir(),
            cancel,
            tracker,
            execution_epoch: 1,
        };
        let runner = Arc::clone(&self.runner);
        let store = self.store.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let outcome = runner.start(context).await;
            finish_run(store, handle, outcome, events);
        });

        let script_path = request
            .script_path
            .unwrap_or_else(|| layout.script_path())
            .display()
            .to_string();
        Ok(LaunchOutcome {
            task_id: run_id.clone(),
            run_id,
            name: request.name,
            display_name,
            script_path,
            message: "workflow launched".to_string(),
        })
    }

    pub(crate) fn pause(
        &self,
        run_id: &str,
        kind: PauseKind,
        message: &str,
    ) -> Result<WorkflowSnapshot, WorkflowControlError> {
        let handle = self.handle(run_id)?;
        let reason = pause_reason(kind);
        {
            let mut tracker = lock(&handle.tracker);
            tracker
                .pause(reason, message)
                .map_err(|_| WorkflowControlError::NotApplicable {
                    run_id: run_id.to_string(),
                    status: tracker.status(),
                    allowed: vec!["resume", "stop"],
                })?;
        }
        self.persist_state(&handle, now_ms());
        Ok(handle.snapshot())
    }

    pub(crate) fn resume(
        &self,
        run_id: &str,
        agent_budget: Option<u64>,
    ) -> Result<WorkflowSnapshot, WorkflowControlError> {
        let handle = self.handle(run_id)?;
        {
            let mut tracker = lock(&handle.tracker);
            tracker
                .resume(agent_budget)
                .map_err(|_| WorkflowControlError::NotApplicable {
                    run_id: run_id.to_string(),
                    status: tracker.status(),
                    allowed: vec!["resume", "stop"],
                })?;
        }
        handle.execution_epoch.fetch_add(1, Ordering::SeqCst);
        self.persist_state(&handle, now_ms());
        Ok(handle.snapshot())
    }

    /// Fixed stop order: cancel scheduling first, then report the terminal state.
    pub(crate) fn stop(&self, run_id: &str) -> Result<WorkflowSnapshot, WorkflowControlError> {
        let handle = self.handle(run_id)?;
        {
            let mut tracker = lock(&handle.tracker);
            tracker
                .stop()
                .map_err(|_| WorkflowControlError::NotApplicable {
                    run_id: run_id.to_string(),
                    status: tracker.status(),
                    allowed: vec!["stop"],
                })?;
        }
        handle.cancel.cancel();
        self.persist_state(&handle, now_ms());
        Ok(handle.snapshot())
    }

    pub(crate) fn snapshot(&self, run_id: &str) -> Result<WorkflowSnapshot, WorkflowControlError> {
        Ok(self.handle(run_id)?.snapshot())
    }

    /// app-server-facing pause; the user-initiated reason is fixed here.
    pub fn pause_run(&self, run_id: &str) -> Result<WorkflowRunSummary, WorkflowControlError> {
        self.pause(run_id, PauseKind::User, "paused by user")?;
        self.run_summary(run_id)
            .ok_or_else(|| WorkflowControlError::RunNotFound(run_id.to_string()))
    }

    pub fn resume_run(&self, run_id: &str) -> Result<WorkflowRunSummary, WorkflowControlError> {
        self.resume(run_id, None)?;
        self.run_summary(run_id)
            .ok_or_else(|| WorkflowControlError::RunNotFound(run_id.to_string()))
    }

    pub fn stop_run(&self, run_id: &str) -> Result<WorkflowRunSummary, WorkflowControlError> {
        self.stop(run_id)?;
        self.run_summary(run_id)
            .ok_or_else(|| WorkflowControlError::RunNotFound(run_id.to_string()))
    }

    pub(crate) fn list(&self) -> Vec<WorkflowSnapshot> {
        let handles: Vec<Arc<RunHandle>> = lock(&self.runs).values().cloned().collect();
        let mut snapshots: Vec<WorkflowSnapshot> =
            handles.iter().map(|handle| handle.snapshot()).collect();
        snapshots.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        snapshots
    }

    /// App-server read view. Completed runs are included only on request.
    pub fn list_runs(&self, include_completed: bool) -> Vec<WorkflowRunSummary> {
        let mut handles: Vec<Arc<RunHandle>> = lock(&self.runs).values().cloned().collect();
        handles.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        handles
            .into_iter()
            .filter(|handle| include_completed || !handle.status().is_terminal())
            .map(|handle| self.summary_for(&handle))
            .collect()
    }

    pub fn run_summary(&self, run_id: &str) -> Option<WorkflowRunSummary> {
        let handle = lock(&self.runs).get(run_id).cloned()?;
        Some(self.summary_for(&handle))
    }

    pub fn script_path(&self, run_id: &str) -> Option<PathBuf> {
        lock(&self.runs)
            .get(run_id)
            .map(|handle| handle.script_path.clone())
    }

    pub fn run_dir(&self, run_id: &str) -> Option<PathBuf> {
        self.paths.run_dir(run_id).ok()
    }

    pub(crate) fn resumable(&self) -> Vec<String> {
        let mut run_ids: Vec<String> = lock(&self.runs)
            .values()
            .filter(|handle| handle.status().can_resume())
            .map(|handle| handle.run_id.clone())
            .collect();
        run_ids.sort();
        run_ids
    }

    /// Create-only save of the frozen script. Never rewrites an existing file.
    pub(crate) fn save(
        &self,
        run_id: &str,
        project_scope: bool,
    ) -> Result<PathBuf, WorkflowControlError> {
        let handle = self.handle(run_id)?;
        let script = std::fs::read_to_string(&handle.script_path).map_err(|error| {
            WorkflowControlError::SaveFailed {
                path: handle.script_path.clone(),
                detail: error.to_string(),
            }
        })?;
        let name = handle.display_name.clone();
        let result = if project_scope {
            registry::save_project_workflow(&self.paths, &name, &script)
        } else {
            registry::save_user_workflow(&self.paths, &name, &script)
        };
        result.map_err(|error| match error {
            registry::RegistryError::SaveExists { path } => WorkflowControlError::SaveExists(path),
            registry::RegistryError::InvalidName(name) => WorkflowControlError::InvalidName(name),
            other => WorkflowControlError::SaveFailed {
                path: self.paths.user_dir().to_path_buf(),
                detail: other.to_string(),
            },
        })
    }

    /// Create-only user-scope save under `$CODEX_HOME/workflows`.
    ///
    /// app-server (`workflow/save`) never exposes the project scope.
    pub fn save_user_workflow(
        &self,
        run_id: &str,
        name: &str,
    ) -> Result<PathBuf, WorkflowControlError> {
        let handle = self.handle(run_id)?;
        let script = std::fs::read_to_string(&handle.script_path).map_err(|error| {
            WorkflowControlError::SaveFailed {
                path: handle.script_path.clone(),
                detail: error.to_string(),
            }
        })?;
        registry::save_user_workflow(&self.paths, name, &script).map_err(|error| match error {
            registry::RegistryError::SaveExists { path } => WorkflowControlError::SaveExists(path),
            registry::RegistryError::InvalidName(name) => WorkflowControlError::InvalidName(name),
            other => WorkflowControlError::SaveFailed {
                path: self.paths.user_dir().to_path_buf(),
                detail: other.to_string(),
            },
        })
    }

    fn summary_for(&self, handle: &RunHandle) -> WorkflowRunSummary {
        let snapshot = handle.snapshot();
        let started_at_ms = lock(&handle.tracker).started_at_ms();
        WorkflowRunSummary {
            run_id: snapshot.run_id,
            name: snapshot.name,
            status: snapshot.status.as_str().to_string(),
            phase: snapshot.current_phase,
            started_at_ms: i64::try_from(started_at_ms).unwrap_or(i64::MAX),
            updated_at_ms: i64::try_from(started_at_ms.saturating_add(snapshot.elapsed_ms))
                .unwrap_or(i64::MAX),
            agent_count: u32::try_from(snapshot.agents.len()).unwrap_or(u32::MAX),
            script_path: handle.script_path.clone(),
            run_dir: self
                .paths
                .run_dir(&handle.run_id)
                .unwrap_or_else(|_| handle.script_path.with_extension("")),
            degraded_fields: snapshot.degraded_fields,
        }
    }

    /// Exposed for the host service: next deterministic task name in a run.
    pub(crate) fn task_name(
        &self,
        run_id: &str,
        source: &str,
    ) -> Result<String, WorkflowControlError> {
        let handle = self.handle(run_id)?;
        Ok(lock(&handle.names).next(source))
    }

    fn handle(&self, run_id: &str) -> Result<Arc<RunHandle>, WorkflowControlError> {
        lock(&self.runs)
            .get(run_id)
            .cloned()
            .ok_or_else(|| WorkflowControlError::RunNotFound(run_id.to_string()))
    }

    fn persist_state(&self, handle: &RunHandle, now_ms: u64) {
        let snapshot = handle.snapshot();
        let state = RunState {
            status: snapshot.status,
            revision: snapshot.revision,
            current_phase: snapshot.current_phase.clone(),
            phases: snapshot.phases.clone(),
            agents_used: snapshot.agents.len() as u64,
            agent_budget: snapshot.budget.total,
            result_summary: snapshot.result_summary.clone(),
            agents: snapshot.agents.clone(),
            updated_at_ms: now_ms,
        };
        let _ = self.store.write_state(&handle.run_id, &state);
        let _ = self
            .store
            .update_manifest_status(&handle.run_id, snapshot.status, now_ms);
        let _ = self.events.send(snapshot);
    }

    fn next_run_id(&self) -> String {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0);
        format!("wf_{:012x}", nanos.wrapping_add(seq))
    }
}

fn finish_run(
    store: Store,
    handle: Arc<RunHandle>,
    outcome: WorkflowOutcome,
    events: broadcast::Sender<WorkflowSnapshot>,
) {
    let now = now_ms();
    {
        let mut tracker = lock(&handle.tracker);
        match outcome {
            WorkflowOutcome::Completed { result } => {
                let summary = summarize(&result);
                let _ = tracker.complete(summary);
            }
            WorkflowOutcome::Paused { kind, message } => {
                let _ = tracker.pause(pause_reason(kind), message);
            }
            WorkflowOutcome::BudgetExceeded { message } => {
                let _ = tracker.budget_limited(Some(message));
            }
            WorkflowOutcome::Cancelled => {
                if !tracker.status().is_terminal() {
                    let _ = tracker.stop();
                }
            }
            WorkflowOutcome::Failed { error } => {
                let _ = tracker.fail(Some(error));
            }
        }
    }
    // Terminal state is persisted before broadcast (design 02 section 4.5).
    let snapshot = handle.snapshot();
    let state = RunState {
        status: snapshot.status,
        revision: snapshot.revision,
        current_phase: snapshot.current_phase.clone(),
        phases: snapshot.phases.clone(),
        agents_used: snapshot.agents.len() as u64,
        agent_budget: snapshot.budget.total,
        result_summary: snapshot.result_summary.clone(),
        agents: snapshot.agents.clone(),
        updated_at_ms: now,
    };
    let _ = store.write_state(&handle.run_id, &state);
    let _ = store.update_manifest_status(&handle.run_id, snapshot.status, now);
    let _ = events.send(snapshot);
}

fn summarize(result: &Value) -> Option<String> {
    if result.is_null() {
        return None;
    }
    let text = result.to_string();
    Some(text.chars().take(512).collect())
}

fn pause_reason(kind: PauseKind) -> PauseReason {
    match kind {
        PauseKind::User => PauseReason::User,
        PauseKind::BackOff => PauseReason::BackOff,
        PauseKind::NoProgress => PauseReason::NoProgress,
        PauseKind::Verification => PauseReason::Verification,
        PauseKind::Infra => PauseReason::Infra,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Marks an agent row terminal when the runner observes its final status.
pub(crate) fn finish_agent(
    tracker: &Mutex<Tracker>,
    agent_id: &str,
    phase: AgentPhase,
    tokens_used: u64,
) {
    lock(tracker).agent_finished(agent_id, phase, tokens_used, now_ms());
}

/// The manager is also the read-only run view used by `RequestService` to
/// resolve `resume`, and the "sole active run" shorthand for pause/stop.
impl RunScriptLookup for WorkflowManager {
    fn find_run_id(&self, key: &str) -> Option<String> {
        let runs = lock(&self.runs);
        if runs.contains_key(key) {
            return Some(key.to_string());
        }
        let mut matches: Vec<&String> = runs.keys().filter(|id| id.starts_with(key)).collect();
        matches.sort();
        match matches.as_slice() {
            [only] => Some((*only).clone()),
            _ => None,
        }
    }

    fn script_copy_for(&self, run_id: &str) -> Option<String> {
        let dir = self.paths.run_dir(run_id).ok()?;
        std::fs::read_to_string(dir.join("script.rhai")).ok()
    }

    fn args_copy_for(&self, run_id: &str) -> Option<Value> {
        let dir = self.paths.run_dir(run_id).ok()?;
        let text = std::fs::read_to_string(dir.join("args.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn active_run_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.runs)
            .values()
            .filter(|handle| !handle.status().is_terminal())
            .map(|handle| handle.run_id.clone())
            .collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
#[path = "run_manager_tests.rs"]
mod tests;
