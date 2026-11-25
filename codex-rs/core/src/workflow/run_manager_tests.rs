//! Tests for the workflow run manager.

use super::*;

use std::sync::atomic::AtomicUsize;

use serde_json::json;
use tempfile::TempDir;

use crate::workflow::manager::MAX_ACTIVE_RUNS;
use crate::workflow::paths::WorkflowPaths;
use crate::workflow::tracker::RunStatus;

struct ScriptedRunner {
    outcome: WorkflowOutcome,
    calls: AtomicUsize,
}

impl ScriptedRunner {
    fn new(outcome: WorkflowOutcome) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            calls: AtomicUsize::new(0),
        })
    }
}

impl WorkflowRunRunner for ScriptedRunner {
    fn start(&self, _context: RunContext) -> RunFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

/// Keeps runs active forever so capacity accounting can be observed.
struct PendingRunner;

impl WorkflowRunRunner for PendingRunner {
    fn start(&self, context: RunContext) -> RunFuture {
        Box::pin(async move {
            context.cancel.cancelled().await;
            WorkflowOutcome::Cancelled
        })
    }
}

fn paths(temp: &TempDir) -> Arc<WorkflowPaths> {
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(codex_home.join("workflows")).expect("home");
    Arc::new(
        WorkflowPaths::resolve_with_git_root(codex_home, Some(temp.path()), "session-1", None)
            .expect("paths"),
    )
}

fn manager(temp: &TempDir, runner: Arc<dyn WorkflowRunRunner>) -> WorkflowManager {
    WorkflowManager::new(paths(temp), runner)
}

fn request() -> LaunchRequest {
    LaunchRequest {
        enabled: true,
        name: "review".into(),
        objective: "review the change".into(),
        script: "fn main() { 1 }".into(),
        script_path: None,
        args: json!({ "target": "HEAD" }),
        effort: Some("medium".into()),
        agent_budget: Some(2),
        phases: vec!["Plan".into(), "Act".into()],
    }
}

async fn settle(manager: &WorkflowManager, run_id: &str) -> WorkflowSnapshot {
    for _ in 0..200 {
        let snapshot = manager.snapshot(run_id).expect("snapshot");
        if snapshot.status.is_terminal() || snapshot.status.is_paused() {
            return snapshot;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("run {run_id} did not settle");
}

#[tokio::test]
async fn launch_persists_a_frozen_run() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Completed {
        result: json!({ "ok": true }),
    });
    let manager = manager(&temp, runner.clone());
    let outcome = manager.launch(request()).expect("launch");
    assert!(outcome.run_id.starts_with("wf_"));

    let layout_root = manager.paths().run_dir(&outcome.run_id).expect("run dir");
    let script = std::fs::read_to_string(layout_root.join("script.rhai")).expect("script");
    assert_eq!(script, "fn main() { 1 }");
    let args = std::fs::read_to_string(layout_root.join("args.json")).expect("args");
    assert!(args.contains("HEAD"));
    let effort = std::fs::read_to_string(layout_root.join("effort")).expect("effort");
    assert_eq!(effort, "medium");

    let settled = settle(&manager, &outcome.run_id).await;
    assert_eq!(settled.status, RunStatus::Complete);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn launch_rejects_when_the_feature_is_off() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Cancelled);
    let manager = manager(&temp, runner.clone());
    let mut request = request();
    request.enabled = false;
    let error = manager.launch(request).expect_err("disabled");
    assert!(matches!(error, WorkflowControlError::Disabled));
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn launch_stops_at_the_active_run_budget() {
    let temp = TempDir::new().expect("temp");
    let manager = manager(&temp, Arc::new(PendingRunner));
    for index in 0..MAX_ACTIVE_RUNS {
        let mut request = request();
        request.name = format!("run-{index}");
        manager.launch(request).expect("launch");
    }
    assert_eq!(manager.active_count(), MAX_ACTIVE_RUNS);
    let error = manager.launch(request()).expect_err("over budget");
    match error {
        WorkflowControlError::MaxActiveRunsExceeded { active, max } => {
            assert_eq!(active, MAX_ACTIVE_RUNS);
            assert_eq!(max, MAX_ACTIVE_RUNS);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn pause_keeps_the_run_and_resume_increments_the_epoch() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Paused {
        kind: PauseKind::User,
        message: "hold".into(),
    });
    let manager = manager(&temp, runner);
    let launched = manager.launch(request()).expect("launch");
    let paused = settle(&manager, &launched.run_id).await;
    assert_eq!(paused.status, RunStatus::UserPaused);
    assert_eq!(manager.active_count(), 0);
    assert_eq!(manager.resumable(), vec![launched.run_id.clone()]);

    let resumed = manager.resume(&launched.run_id, Some(4)).expect("resume");
    assert_eq!(resumed.status, RunStatus::Active);
    // Resume only re-opens the run; the budget is raised separately and only
    // when the run is `budget_limited` (design 02 section 4.3 rule 5).
    assert_eq!(resumed.budget.total, Some(2));
}

#[tokio::test]
async fn stop_is_terminal_and_idempotent() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Paused {
        kind: PauseKind::User,
        message: "hold".into(),
    });
    let manager = manager(&temp, runner);
    let launched = manager.launch(request()).expect("launch");
    settle(&manager, &launched.run_id).await;
    let stopped = manager.stop(&launched.run_id).expect("stop");
    assert_eq!(stopped.status, RunStatus::Cancelled);
    let error = manager.stop(&launched.run_id).expect_err("already stopped");
    assert!(matches!(
        error,
        WorkflowControlError::NotApplicable {
            status: RunStatus::Cancelled,
            ..
        }
    ));
}

#[tokio::test]
async fn unknown_run_ids_are_reported() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Cancelled);
    let manager = manager(&temp, runner);
    assert!(matches!(
        manager.snapshot("wf_missing").expect_err("missing"),
        WorkflowControlError::RunNotFound(_)
    ));
}

#[tokio::test]
async fn task_names_are_scoped_to_one_run() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Paused {
        kind: PauseKind::User,
        message: "hold".into(),
    });
    let manager = manager(&temp, runner);
    let first = manager.launch(request()).expect("launch");
    let mut second_request = request();
    second_request.name = "second".into();
    let second = manager.launch(second_request).expect("launch");
    assert_eq!(
        manager.task_name(&first.run_id, "Plan").expect("name"),
        "plan"
    );
    assert_eq!(
        manager.task_name(&first.run_id, "Plan").expect("name"),
        "plan_2"
    );
    assert_eq!(
        manager.task_name(&second.run_id, "Plan").expect("name"),
        "plan"
    );
}

#[tokio::test]
async fn events_are_broadcast_after_persistence() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Completed {
        result: json!(null),
    });
    let manager = manager(&temp, runner);
    let mut events = manager.subscribe();
    let launched = manager.launch(request()).expect("launch");
    let mut terminal = None;
    for _ in 0..50 {
        let snapshot = events.recv().await.expect("event");
        assert!(manager.paths().run_dir(&snapshot.run_id).is_ok());
        if snapshot.status.is_terminal() {
            terminal = Some(snapshot);
            break;
        }
    }
    let terminal = terminal.expect("terminal event");
    assert_eq!(terminal.run_id, launched.run_id);
    let state = manager
        .store
        .read_state(&launched.run_id)
        .expect("state")
        .expect("state present");
    assert_eq!(state.status, terminal.status);
}

/// TASK-030/031 bridge: a prepared workflow carries name/phases/objective and
/// adopts the configured default budget when the caller omits one.
#[tokio::test]
async fn launch_prepared_uses_meta_and_default_budget() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Completed {
        result: json!({ "ok": true }),
    });
    let limits = WorkflowLimits {
        max_active_runs: 4,
        default_agent_budget: 9,
        max_concurrent_agents: 2,
    };
    let manager = WorkflowManager::with_limits(paths(&temp), runner.clone(), limits);
    assert_eq!(manager.limits(), limits);

    let workflow = ResolvedWorkflow {
        meta: codex_workflow::WorkflowMeta {
            name: "hello".into(),
            description: "say hello".into(),
            when_to_use: None,
            phases: vec![codex_workflow::meta::PhaseMeta {
                title: "Plan".into(),
                detail: None,
            }],
        },
        script: "fn main() { 1 }".into(),
        origin: WorkflowOrigin::Inline,
        scope: None,
    };
    let outcome = manager
        .launch_prepared(workflow, json!({ "objective": "ship it" }), None, true)
        .expect("launch");
    assert_eq!(outcome.name, "hello");
    let snapshot = settle(&manager, &outcome.run_id).await;
    assert_eq!(snapshot.status, RunStatus::Complete);
    assert_eq!(snapshot.budget.total, Some(9));
    assert_eq!(snapshot.objective, "ship it");
    assert_eq!(snapshot.phases, vec!["Plan".to_string()]);
}

/// Control routing resolves a unique run when the caller omits `run_id`.
#[tokio::test]
async fn control_resolves_the_sole_run() {
    let temp = TempDir::new().expect("temp");
    let runner = ScriptedRunner::new(WorkflowOutcome::Paused {
        kind: PauseKind::User,
        message: "hold".into(),
    });
    let manager = manager(&temp, runner);
    let launched = manager.launch(request()).expect("launch");
    settle(&manager, &launched.run_id).await;

    let lookup: &dyn RunScriptLookup = &manager;
    assert_eq!(lookup.active_run_ids(), vec![launched.run_id.clone()]);
    assert_eq!(
        lookup.find_run_id(&launched.run_id[..6]).as_deref(),
        Some(launched.run_id.as_str())
    );
    let stopped = manager
        .control(ControlAction::Stop, &launched.run_id)
        .expect("stop");
    assert_eq!(stopped.status, RunStatus::Cancelled);
}
