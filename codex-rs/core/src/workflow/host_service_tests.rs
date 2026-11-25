//! Tests for the workflow host service.

use super::*;

use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;

use crate::workflow::paths::WorkflowPaths;
use crate::workflow::tracker::RunStatus;
use crate::workflow::tracker::Tracker;

struct FakeSpawner {
    result: Result<AgentResult, HostError>,
}

impl AgentSpawner for FakeSpawner {
    fn resolve_workflow(&self, name: &str) -> Result<String, HostError> {
        Ok(format!("script:{name}"))
    }

    fn spawn_agent(&self, request: SpawnRequest) -> SpawnFuture {
        let result = self.result.clone();
        let _ = request;
        Box::pin(async move { result })
    }

    fn git_diff_since(&self, commit: &str) -> Result<String, HostError> {
        Ok(format!("diff:{commit}"))
    }
}

fn harness(
    result: Result<AgentResult, HostError>,
) -> (TempDir, WorkflowHostService, Arc<Mutex<Tracker>>, Store) {
    let temp = TempDir::new().expect("temp");
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(codex_home.join("workflows")).expect("home");
    let paths = Arc::new(
        WorkflowPaths::resolve_with_git_root(codex_home, Some(temp.path()), "session-1", None)
            .expect("paths"),
    );
    let store = Store::new(paths.session_root().to_path_buf());
    store
        .create_run(
            "wf_1",
            "demo",
            "demo objective",
            "fn main() {}",
            "{}",
            None,
            1,
        )
        .expect("run");
    let tracker = Arc::new(Mutex::new(Tracker::new(
        "wf_1",
        "demo",
        "demo objective",
        vec!["Plan".into()],
        Some(4),
        true,
        1,
    )));
    let registry = Arc::new(WorkflowRegistry::new(Arc::clone(&paths)).expect("registry"));
    let service = WorkflowHostService::new(
        "wf_1".into(),
        Arc::clone(&tracker),
        store.clone(),
        registry,
        Arc::new(FakeSpawner { result }),
        CancellationToken::new(),
        2,
    );
    (temp, service, tracker, store)
}

#[tokio::test]
async fn reserve_and_release_update_the_budget_view() {
    let (_temp, service, tracker, _store) = harness(Err(HostError::Cancelled));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ReserveAgentCalls {
        count: 2,
        reply: tx,
    });
    rx.await.expect("reply").expect("reserve");
    assert_eq!(lock(&tracker).snapshot().budget.reserved, 2);

    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ReleaseAgentCalls {
        count: 1,
        reply: tx,
    });
    rx.await.expect("reply").expect("release");
    assert_eq!(lock(&tracker).snapshot().budget.reserved, 1);
}

#[tokio::test]
async fn the_logical_agent_call_cap_is_enforced() {
    let (_temp, service, _tracker, _store) = harness(Err(HostError::Cancelled));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ReserveAgentCalls {
        count: MAX_AGENT_BUDGET * 2,
        reply: tx,
    });
    rx.await.expect("reply").expect("first tenant");
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ReserveAgentCalls {
        count: 1,
        reply: tx,
    });
    let error = rx.await.expect("reply").expect_err("quota");
    assert!(matches!(error, HostError::AgentCallQuotaExceeded { .. }));
}

#[tokio::test]
async fn phase_log_and_telemetry_land_in_the_snapshot() {
    let (_temp, service, tracker, _store) = harness(Err(HostError::Cancelled));
    service.dispatch(WorkflowHostRequest::Phase {
        title: "Research".into(),
        replayed: false,
    });
    service.dispatch(WorkflowHostRequest::Log {
        message: "hello".into(),
        replayed: false,
    });
    service.dispatch(WorkflowHostRequest::Telemetry {
        name: "tokens".into(),
        fields: json!({"n": 3}),
        replayed: false,
    });
    let snapshot = lock(&tracker).snapshot();
    assert_eq!(snapshot.current_phase.as_deref(), Some("Research"));
    assert_eq!(snapshot.last_event.as_deref(), Some("tokens"));
    assert!(snapshot.history.iter().any(|row| row.kind == "log"));
}

#[tokio::test]
async fn budget_query_returns_the_tracker_view() {
    let (_temp, service, _tracker, _store) = harness(Err(HostError::Cancelled));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::BudgetQuery { reply: tx });
    let budget = rx.await.expect("reply").expect("budget");
    assert_eq!(budget.total, Some(4));
    assert_eq!(budget.remaining, Some(4));
}

#[tokio::test]
async fn scratch_files_round_trip_inside_the_run() {
    let (_temp, service, _tracker, _store) = harness(Err(HostError::Cancelled));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::WriteScratchFile {
        name: "report.md".into(),
        content: "# report\n".into(),
        reply: tx,
    });
    let path = rx.await.expect("reply").expect("write");
    assert!(path.ends_with("report.md"));

    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ReadScratchFile {
        name: "report.md".into(),
        reply: tx,
    });
    assert_eq!(rx.await.expect("reply").expect("read"), "# report\n");

    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ReadScratchFile {
        name: "../escape".into(),
        reply: tx,
    });
    assert!(rx.await.expect("reply").is_err());
}

#[tokio::test]
async fn templates_render_and_unknown_names_fail() {
    let (_temp, service, _tracker, _store) = harness(Err(HostError::Cancelled));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::RenderTemplate {
        name: "plan".into(),
        vars: json!({"objective": "ship"}),
        reply: tx,
    });
    let rendered = rx.await.expect("reply").expect("render");
    assert!(rendered.contains("ship"));

    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::RenderTemplate {
        name: "nope".into(),
        vars: json!({}),
        reply: tx,
    });
    assert!(rx.await.expect("reply").is_err());
}

#[tokio::test]
async fn spawn_agent_returns_the_spawner_result() {
    let (_temp, service, tracker, _store) = harness(Ok(AgentResult {
        agent_id: "agent-1".into(),
        success: true,
        output: json!({"greeting": "hi"}),
        cancelled: false,
        tokens_used: 5,
        duration_ms: 10,
    }));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::SpawnAgent {
        opts: AgentOpts {
            prompt: "say hi".into(),
            label: Some("greeter".into()),
            phase: Some("Greet".into()),
            ..AgentOpts::default()
        },
        reply: tx,
    });
    let result = rx.await.expect("reply").expect("spawn");
    assert_eq!(result.agent_id, "agent-1");
    let snapshot = lock(&tracker).snapshot();
    assert!(snapshot.agents.iter().any(|row| row.label == "greeter"));
}

#[tokio::test]
async fn cancelled_runs_reject_new_spawns() {
    let temp = TempDir::new().expect("temp");
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(codex_home.join("workflows")).expect("home");
    let paths = Arc::new(
        WorkflowPaths::resolve_with_git_root(codex_home, Some(temp.path()), "s", None)
            .expect("paths"),
    );
    let store = Store::new(paths.session_root().to_path_buf());
    let tracker = Arc::new(Mutex::new(Tracker::new(
        "wf_1",
        "demo",
        "objective",
        vec![],
        Some(1),
        true,
        1,
    )));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let service = WorkflowHostService::new(
        "wf_1".into(),
        tracker,
        store,
        Arc::new(WorkflowRegistry::new(paths).expect("registry")),
        Arc::new(FakeSpawner {
            result: Ok(AgentResult {
                agent_id: "never".into(),
                success: true,
                output: json!(null),
                cancelled: false,
                tokens_used: 0,
                duration_ms: 0,
            }),
        }),
        cancel,
        2,
    );
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::SpawnAgent {
        opts: AgentOpts::default(),
        reply: tx,
    });
    assert!(matches!(
        rx.await.expect("reply"),
        Err(HostError::Cancelled)
    ));
}

#[tokio::test]
async fn resolve_and_diff_delegate_to_the_spawner() {
    let (_temp, service, _tracker, _store) = harness(Err(HostError::Cancelled));
    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::ResolveWorkflow {
        name: "hello".into(),
        reply: tx,
    });
    assert_eq!(rx.await.expect("reply").expect("resolve"), "script:hello");

    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::GitDiffSince {
        commit: "abc".into(),
        reply: tx,
    });
    assert_eq!(rx.await.expect("reply").expect("diff"), "diff:abc");
}

#[tokio::test]
async fn tracker_starts_active_with_the_configured_budget() {
    let (_temp, _service, tracker, _store) = harness(Err(HostError::Cancelled));
    let snapshot = lock(&tracker).snapshot();
    assert_eq!(snapshot.status, RunStatus::Active);
    assert_eq!(snapshot.budget.total, Some(4));
}

#[tokio::test]
async fn unsupported_agent_opts_fail_before_spawning_with_the_field_name() {
    let (_temp, service, _tracker, _store) = harness(Err(HostError::Cancelled));
    let cases = [
        (
            AgentOpts {
                max_output_tokens: Some(10),
                ..AgentOpts::default()
            },
            "max_output_tokens",
        ),
        (
            AgentOpts {
                capability_mode: Some("read-only".into()),
                ..AgentOpts::default()
            },
            "capability_mode",
        ),
        (
            AgentOpts {
                isolation_worktree: true,
                ..AgentOpts::default()
            },
            "isolation_worktree",
        ),
        (
            AgentOpts {
                fork_context: true,
                ..AgentOpts::default()
            },
            "fork_context",
        ),
        (
            AgentOpts {
                resume_from: Some("wf_0".into()),
                ..AgentOpts::default()
            },
            "resume_from",
        ),
    ];
    for (opts, field) in cases {
        let (tx, rx) = oneshot::channel();
        service.dispatch(WorkflowHostRequest::SpawnAgent { opts, reply: tx });
        let error = rx.await.expect("reply").expect_err("unsupported field");
        assert_eq!(
            error.to_string(),
            format!("unsupported in this context: {field}")
        );
    }

    let (tx, rx) = oneshot::channel();
    service.dispatch(WorkflowHostRequest::SpawnAgent {
        opts: AgentOpts {
            isolation_worktree: false,
            ..AgentOpts::default()
        },
        reply: tx,
    });
    assert!(matches!(
        rx.await.expect("reply"),
        Err(HostError::Cancelled)
    ));
}
