//! Tests for the workflow engine.

use super::*;
use crate::host::{AgentResult, BudgetState};

fn spawn_mock_host(
    mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
    mut on_request: impl FnMut(WorkflowHostRequest) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Some(req) = rx.blocking_recv() {
            match req {
                WorkflowHostRequest::ReserveAgentCalls { reply, .. }
                | WorkflowHostRequest::ReleaseAgentCalls { reply, .. } => {
                    let _ = reply.send(Ok(()));
                }
                other => on_request(other),
            }
        }
    })
}

fn spawn_budget_tracking_host(
    mut rx: mpsc::UnboundedReceiver<WorkflowHostRequest>,
    agents_used: std::sync::Arc<std::sync::atomic::AtomicU64>,
    mut on_request: impl FnMut(WorkflowHostRequest) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Some(req) = rx.blocking_recv() {
            match req {
                WorkflowHostRequest::ReserveAgentCalls { count, reply } => {
                    agents_used.fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                    let _ = reply.send(Ok(()));
                }
                WorkflowHostRequest::ReleaseAgentCalls { count, reply } => {
                    let _ = agents_used.try_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |used| Some(used.saturating_sub(count)),
                    );
                    let _ = reply.send(Ok(()));
                }
                other => on_request(other),
            }
        }
    })
}

fn agent_result(output: &str) -> AgentResult {
    AgentResult {
        agent_id: "child-1".into(),
        success: true,
        output: serde_json::Value::String(output.into()),
        cancelled: false,
        tokens_used: 10,
        duration_ms: 5,
    }
}

fn params(
    script: &str,
    journal: Journal,
    host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
) -> WorkflowRunParams {
    WorkflowRunParams {
        script: script.to_string(),
        args: serde_json::json!({ "objective": "test" }),
        journal,
        host_tx,
        cancel: CancellationToken::new(),
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
    }
}

fn legacy_spawn_agent_hash(opts: &AgentOpts) -> String {
    let mut payload = serde_json::to_value(opts).unwrap();
    assert!(payload.as_object_mut().unwrap().remove("effort").is_some());
    request_hash("spawn_agent", &payload)
}

#[test]
fn happy_path_completes_with_agent_output() {
    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| match req {
        WorkflowHostRequest::SpawnAgent { reply, .. } => {
            let _ = reply.send(Ok(agent_result("agent says hi")));
        }
        WorkflowHostRequest::Phase { .. } | WorkflowHostRequest::Log { .. } => {}
        other => panic!("unexpected request: {other:?}"),
    });

    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            phase("Work");
            let r = agent("do it", #{ label: "worker" });
            complete(r.output);
            "#,
        Journal::new(None),
        tx,
    ));
    drop(host);

    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("agent says hi"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn workflow_calls_resolved_script_with_args() {
    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| match req {
        WorkflowHostRequest::ResolveWorkflow { name, reply } => {
            assert_eq!(name, "increment");
            let _ = reply.send(Ok(
                r#"let meta = #{ name: "increment", description: "increment" };
                       complete(args.value + 1);"#
                    .into(),
            ));
        }
        other => panic!("unexpected request: {other:?}"),
    });

    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            complete(workflow("increment", #{ value: 4 }) * 2);
            "#,
        Journal::new(None),
        tx,
    ));
    drop(host);

    assert!(matches!(
        outcome,
        WorkflowOutcome::Completed { result } if result == serde_json::json!(10)
    ));
}

#[test]
fn pipeline_chains_stages_with_original_item_and_index() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            complete(pipeline(
                [1, 2, 3],
                |previous, item, index| previous + item + index,
                |previous, item, index| previous * 10 + item + index
            ));
            "#,
        Journal::new(None),
        tx,
    ));

    assert!(matches!(
        outcome,
        WorkflowOutcome::Completed { result } if result == serde_json::json!([21, 53, 85])
    ));
}

#[test]
fn catchable_host_failure_journals_and_replays() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            let d = "";
            try { d = git_diff_since("abc"); } catch (e) { d = "fallback"; }
            let r = agent("work");
            complete(r.output + ":" + d);
        "#;

    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| match req {
        WorkflowHostRequest::GitDiffSince { reply, .. } => {
            let _ = reply.send(Err(crate::host::HostError::Failed("boom".into())));
        }
        WorkflowHostRequest::SpawnAgent { reply, .. } => {
            let _ = reply.send(Ok(agent_result("one")));
        }
        WorkflowHostRequest::Phase { .. } | WorkflowHostRequest::Log { .. } => {}
        other => panic!("unexpected request: {other:?}"),
    });
    let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
    drop(host);
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("one:fallback"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| match req {
        WorkflowHostRequest::Phase { .. } | WorkflowHostRequest::Log { .. } => {}
        other => panic!("replay must not hit the host: {other:?}"),
    });
    let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
    drop(host);
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("one:fallback"));
        }
        other => panic!("expected replayed Completed, got {other:?}"),
    }
}

#[test]
fn await_user_pauses_once_then_passes_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            await_user("back_off", "needs a human");
            complete("resumed");
        "#;

    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
    match outcome {
        WorkflowOutcome::Paused { kind, message } => {
            assert_eq!(kind, PauseKind::BackOff);
            assert_eq!(message, "needs a human");
        }
        other => panic!("expected Paused, got {other:?}"),
    }

    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("resumed"));
        }
        other => panic!("expected Completed after resume, got {other:?}"),
    }
}

#[test]
fn timestamp_is_blocked_for_determinism() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            let t = timestamp();
            complete("unreachable");
            "#,
        Journal::new(None),
        tx,
    ));
    match outcome {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("deterministic"), "unexpected error: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn args_are_visible_to_script() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            complete(args.objective);
            "#,
        Journal::new(None),
        tx,
    ));
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("test"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn pause_maps_kind() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            pause("back_off", "too many rejections");
            "#,
        Journal::new(None),
        tx,
    ));
    match outcome {
        WorkflowOutcome::Paused { kind, message } => {
            assert_eq!(kind, PauseKind::BackOff);
            assert_eq!(message, "too many rejections");
        }
        other => panic!("expected Paused, got {other:?}"),
    }
}

#[test]
fn cancellation_wins_over_pure_loop() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut p = params(
        r#"
            let meta = #{ name: "t", description: "d" };
            let x = 0;
            loop { x += 1; }
            "#,
        Journal::new(None),
        tx,
    );
    p.cancel = cancel;
    assert!(matches!(run_workflow(p), WorkflowOutcome::Cancelled));
}

#[test]
fn parallel_rejects_oversized_fanout_before_spawning() {
    let (tx, rx) = mpsc::unbounded_channel();
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests_in_host = requests.clone();
    let host = spawn_mock_host(rx, move |req| {
        requests_in_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            let _ = reply.send(Ok(agent_result("unexpected")));
        }
    });
    let script = format!(
        r#"
            let meta = #{{ name: "t", description: "d" }};
            let jobs = [];
            for i in 0..{} {{ jobs.push(#{{ prompt: "job" + i.to_string() }}); }}
            parallel(jobs);
            "#,
        MAX_PARALLEL + 1
    );
    let outcome = run_workflow(params(&script, Journal::new(None), tx));
    drop(host);
    match outcome {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("parallel() accepts at most"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn host_call_limit_is_non_catchable_and_prevents_sends() {
    let (tx, rx) = mpsc::unbounded_channel();
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests_in_host = requests.clone();
    let host = spawn_mock_host(rx, move |req| {
        requests_in_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let WorkflowHostRequest::BudgetQuery { reply } = req {
            let _ = reply.send(Ok(BudgetState {
                total: None,
                spent: 0,
                reserved: 0,
                remaining: None,
            }));
        }
    });
    let mut journal = Journal::new(None);
    let hash = request_hash("budget", &serde_json::Value::Null);
    for seq in 0..MAX_HOST_CALLS {
        journal
            .record(
                seq,
                "budget",
                hash.clone(),
                serde_json::json!({ "total": null, "spent": 0, "reserved": 0, "remaining": null }),
            )
            .unwrap();
    }
    let script = format!(
        r#"
            let meta = #{{ name: "t", description: "d" }};
            for i in 0..{MAX_HOST_CALLS} {{ budget(); }}
            try {{ budget(); }} catch (e) {{ complete("caught"); }}
            complete("unreachable");
            "#
    );
    let outcome = run_workflow(params(&script, journal, tx));
    drop(host);
    match outcome {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("maximum of"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn parallel_setup_error_drains_already_sent_replies() {
    let (tx, rx) = mpsc::unbounded_channel();
    let first_reply_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first_reply_in_host = first_reply_observed.clone();
    let host = spawn_mock_host(rx, move |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            first_reply_in_host.store(
                reply.send(Ok(agent_result("drained"))).is_ok(),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    });
    let mut journal = Journal::new(None);
    let hash = request_hash("budget", &serde_json::Value::Null);
    for seq in 0..MAX_HOST_CALLS - 1 {
        journal
            .record(
                seq,
                "budget",
                hash.clone(),
                serde_json::json!({ "total": null, "spent": 0, "reserved": 0, "remaining": null }),
            )
            .unwrap();
    }
    let script = format!(
        r#"
            let meta = #{{ name: "t", description: "d" }};
            for i in 0..{} {{ budget(); }}
            parallel([#{{ prompt: "live" }}, #{{ prompt: "over-limit" }}]);
            "#,
        MAX_HOST_CALLS - 1
    );
    let outcome = run_workflow(params(&script, journal, tx));
    drop(host);
    match outcome {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("maximum of"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(first_reply_observed.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn parallel_budget_exceeded_leaves_panel_unjournaled_for_raised_cap_resume() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let (tx, rx) = mpsc::unbounded_channel();
    let second_reply_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_reply_in_host = second_reply_observed.clone();
    let mut request_index = 0;
    let host = spawn_mock_host(rx, move |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            if request_index == 0 {
                let _ = reply.send(Err(HostError::BudgetExceeded));
            } else {
                second_reply_in_host.store(
                    reply.send(Ok(agent_result("drained"))).is_ok(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
            request_index += 1;
        }
    });
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            parallel([#{ prompt: "first" }, #{ prompt: "second" }]);
        "#;
    let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
    drop(host);
    assert!(matches!(outcome, WorkflowOutcome::BudgetExceeded { .. }));
    assert!(second_reply_observed.load(std::sync::atomic::Ordering::SeqCst));

    let journal = Journal::load(journal_path).unwrap();
    assert_eq!(
        journal.len(),
        0,
        "resumable budget terminal must not journal the parallel panel"
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let live_again = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let live_again_host = live_again.clone();
    let host = spawn_mock_host(rx, move |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            live_again_host.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = reply.send(Ok(agent_result("after raise")));
        }
    });
    let replay = run_workflow(params(script, journal, tx));
    drop(host);
    assert!(matches!(replay, WorkflowOutcome::Completed { .. }));
    assert_eq!(live_again.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn cancelled_live_agent_releases_budget_so_resume_does_not_double_charge() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let agents_used = std::sync::Arc::new(AtomicU64::new(0));
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            let r = agent("work");
            complete(r.output);
        "#;

    {
        let used = agents_used.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_budget_tracking_host(rx, used, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Err(HostError::Cancelled));
            }
        });
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::Cancelled));
        assert_eq!(
            agents_used.load(Ordering::SeqCst),
            0,
            "cancelled agent must ReleaseAgentCalls the reserved slot"
        );
        assert_eq!(
            Journal::load(journal_path.clone()).unwrap().len(),
            0,
            "cancelled agent must leave the spawn unjournaled"
        );
    }

    {
        let used = agents_used.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_budget_tracking_host(rx, used, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("after resume")));
            }
        });
        let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("after resume"));
            }
            other => panic!("expected Completed after resume, got {other:?}"),
        }
        assert_eq!(
            agents_used.load(Ordering::SeqCst),
            1,
            "resume must not double-charge a slot left over from cancelled first run"
        );
    }
}

#[test]
fn cancelled_parallel_releases_budget_so_resume_does_not_double_charge() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let agents_used = std::sync::Arc::new(AtomicU64::new(0));
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([#{ prompt: "first" }, #{ prompt: "second" }]);
            complete(results);
        "#;

    {
        let used = agents_used.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_budget_tracking_host(rx, used, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Err(HostError::Cancelled));
            }
        });
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::Cancelled));
        assert_eq!(
            agents_used.load(Ordering::SeqCst),
            0,
            "cancelled parallel must release live_count reserved slots"
        );
        assert_eq!(Journal::load(journal_path.clone()).unwrap().len(), 0);
    }

    {
        let used = agents_used.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_budget_tracking_host(rx, used, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("ok")));
            }
        });
        let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::Completed { .. }));
        assert_eq!(
            agents_used.load(Ordering::SeqCst),
            2,
            "resume must charge exactly live_count, not leftover + live_count"
        );
    }
}

#[test]
fn budget_exceeded_live_agent_releases_budget_so_resume_does_not_double_charge() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let agents_used = std::sync::Arc::new(AtomicU64::new(0));
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            let r = agent("work");
            complete(r.output);
        "#;

    {
        let used = agents_used.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_budget_tracking_host(rx, used, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Err(HostError::BudgetExceeded));
            }
        });
        let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
        drop(host);
        assert!(matches!(outcome, WorkflowOutcome::BudgetExceeded { .. }));
        assert_eq!(
            agents_used.load(Ordering::SeqCst),
            0,
            "budget-exceeded agent must ReleaseAgentCalls the reserved slot"
        );
        assert_eq!(Journal::load(journal_path.clone()).unwrap().len(), 0);
    }

    {
        let used = agents_used.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let host = spawn_budget_tracking_host(rx, used, |req| {
            if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
                let _ = reply.send(Ok(agent_result("after raise")));
            }
        });
        let outcome = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
        drop(host);
        match outcome {
            WorkflowOutcome::Completed { result } => {
                assert_eq!(result, serde_json::json!("after raise"));
            }
            other => panic!("expected Completed after resume, got {other:?}"),
        }
        assert_eq!(
            agents_used.load(Ordering::SeqCst),
            1,
            "resume after budget terminal must not double-charge"
        );
    }
}

#[test]
fn parallel_journals_soft_failure_null_and_later_success() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let (tx, rx) = mpsc::unbounded_channel();
    let mut request_index = 0;
    let host = spawn_mock_host(rx, move |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            if request_index == 0 {
                let _ = reply.send(Err(HostError::Failed("boom".into())));
            } else {
                let _ = reply.send(Ok(agent_result("ok")));
            }
            request_index += 1;
        }
    });
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([#{ prompt: "first" }, #{ prompt: "second" }]);
            complete(results);
        "#;
    let outcome = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
    drop(host);
    assert!(matches!(outcome, WorkflowOutcome::Completed { .. }));
    let journal = Journal::load(journal_path).unwrap();
    assert_eq!(journal.len(), 2);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let replay = run_workflow(params(script, journal, tx));
    assert!(matches!(replay, WorkflowOutcome::Completed { .. }));
    assert!(
        rx.try_recv().is_err(),
        "dense soft-failure replay must not reexecute either sibling"
    );
}

#[test]
fn parallel_replays_catchable_failure_sentinel() {
    let mut journal = Journal::new(None);
    let opts = AgentOpts {
        prompt: "replayed".into(),
        ..Default::default()
    };
    let hash = request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap());
    journal
        .record(
            0,
            "spawn_agent",
            hash,
            host_error_sentinel("replayed failure"),
        )
        .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            try {
                parallel([#{ prompt: "replayed" }]);
                complete("not caught");
            } catch (e) {
                complete("caught:" + e);
            }
            "#,
        journal,
        tx,
    ));
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("caught:replayed failure"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn parallel_preserves_order_and_nulls_failures() {
    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| {
        if let WorkflowHostRequest::SpawnAgent { opts, reply } = req {
            if opts.prompt.contains("fail") {
                let _ = reply.send(Err(HostError::Failed("boom".into())));
            } else {
                let _ = reply.send(Ok(agent_result(&format!("ok:{}", opts.prompt))));
            }
        }
    });
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([
                #{ prompt: "a" },
                #{ prompt: "fail-b" },
                #{ prompt: "c" },
            ]);
            let summary = results.map(|r| if r == () { "null" } else { r.output });
            complete(summary);
            "#,
        Journal::new(None),
        tx,
    ));
    drop(host);
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!(["ok:a", "null", "ok:c"]));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn agent_replays_legacy_hash_without_effort() {
    let opts = AgentOpts {
        prompt: "legacy single".into(),
        effort: Some("high".into()),
        ..Default::default()
    };
    let legacy_hash = legacy_spawn_agent_hash(&opts);
    assert_ne!(
        legacy_hash,
        request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap())
    );
    let mut journal = Journal::new(None);
    journal
        .record(
            0,
            "spawn_agent",
            legacy_hash,
            serde_json::to_value(agent_result("legacy result")).unwrap(),
        )
        .unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            let result = agent("legacy single", #{ effort: "high" });
            complete(result.output);
            "#,
        journal,
        tx,
    ));

    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("legacy result"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "replay must not hit the host");
}

#[test]
fn parallel_replays_legacy_hashes_without_effort() {
    let opts = [
        AgentOpts {
            prompt: "legacy first".into(),
            effort: Some("low".into()),
            ..Default::default()
        },
        AgentOpts {
            prompt: "legacy second".into(),
            effort: Some("high".into()),
            ..Default::default()
        },
    ];
    let mut journal = Journal::new(None);
    for (seq, (opts, output)) in opts
        .iter()
        .zip(["first result", "second result"])
        .enumerate()
    {
        journal
            .record(
                seq as u64,
                "spawn_agent",
                legacy_spawn_agent_hash(opts),
                serde_json::to_value(agent_result(output)).unwrap(),
            )
            .unwrap();
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            let results = parallel([
                #{ prompt: "legacy first", effort: "low" },
                #{ prompt: "legacy second", effort: "high" },
            ]);
            complete(results.map(|result| result.output));
            "#,
        journal,
        tx,
    ));

    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!(["first result", "second result"]));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "replay must not hit the host");
}

#[test]
fn new_agent_recording_hash_includes_effort() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            let _ = reply.send(Ok(agent_result("recorded")));
        }
    });
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            agent("new recording", #{ effort: "high" });
            complete("ok");
            "#,
        Journal::new(Some(journal_path.clone())),
        tx,
    ));
    drop(host);
    assert!(matches!(outcome, WorkflowOutcome::Completed { .. }));

    let opts = AgentOpts {
        prompt: "new recording".into(),
        effort: Some("high".into()),
        ..Default::default()
    };
    let journal = Journal::load(journal_path).unwrap();
    let current_hash = request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap());
    assert!(
        journal
            .replay(0, "spawn_agent", &current_hash)
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        journal.replay(0, "spawn_agent", &legacy_spawn_agent_hash(&opts)),
        Err(JournalError::Divergence { .. })
    ));
}

#[test]
fn agent_current_hash_mismatch_still_diverges() {
    let opts = AgentOpts {
        prompt: "original prompt".into(),
        effort: Some("high".into()),
        ..Default::default()
    };
    let mut journal = Journal::new(None);
    journal
        .record(
            0,
            "spawn_agent",
            request_hash("spawn_agent", &serde_json::to_value(&opts).unwrap()),
            serde_json::to_value(agent_result("must not replay")).unwrap(),
        )
        .unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            agent("edited prompt", #{ effort: "high" });
            "#,
        journal,
        tx,
    ));

    match outcome {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("divergence"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        rx.try_recv().is_err(),
        "divergent replay must not hit the host"
    );
}

#[test]
fn journal_replay_skips_host_calls() {
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            let a = agent("first");
            let b = budget();
            complete(#{ out: a.output, spent: b.spent, reserved: b.reserved, remaining: b.remaining });
        "#;

    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");

    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| match req {
        WorkflowHostRequest::SpawnAgent { reply, .. } => {
            let _ = reply.send(Ok(agent_result("recorded output")));
        }
        WorkflowHostRequest::BudgetQuery { reply } => {
            let _ = reply.send(Ok(BudgetState {
                total: Some(1000),
                spent: 123,
                reserved: 100,
                remaining: Some(777),
            }));
        }
        _ => {}
    });
    let first = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
    drop(host);
    let WorkflowOutcome::Completed { result: first } = first else {
        panic!("first run should complete");
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| match req {
        WorkflowHostRequest::SpawnAgent { .. } | WorkflowHostRequest::BudgetQuery { .. } => {
            panic!("replay must not hit the host")
        }
        _ => {}
    });
    let second = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
    drop(host);
    let WorkflowOutcome::Completed { result: second } = second else {
        panic!("resumed run should complete");
    };
    assert_eq!(first, second);
    assert_eq!(second.get("spent"), Some(&serde_json::json!(123)));
    assert_eq!(second.get("reserved"), Some(&serde_json::json!(100)));
    assert_eq!(second.get("remaining"), Some(&serde_json::json!(777)));
}

#[test]
fn journal_write_failure_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");
    std::fs::create_dir(&journal_path).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            let _ = reply.send(Ok(agent_result("unpersisted")));
        }
    });
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            agent("work");
            complete("must not complete");
            "#,
        Journal::new(Some(journal_path)),
        tx,
    ));
    drop(host);
    match outcome {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("journal io"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn journal_divergence_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");

    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            let _ = reply.send(Ok(agent_result("v1")));
        }
    });
    let first = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            agent("original prompt");
            complete("ok");
            "#,
        Journal::new(Some(journal_path.clone())),
        tx,
    ));
    drop(host);
    assert!(matches!(first, WorkflowOutcome::Completed { .. }));

    let (tx, _rx) = mpsc::unbounded_channel();
    let second = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            agent("EDITED prompt");
            complete("ok");
            "#,
        Journal::load(journal_path).unwrap(),
        tx,
    ));
    match second {
        WorkflowOutcome::Failed { error } => {
            assert!(error.contains("divergence"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn phase_carries_replay_flag() {
    let script = r#"
            let meta = #{ name: "t", description: "d" };
            phase("One");
            agent("x");
            phase("Two");
            complete("ok");
        "#;
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("journal.jsonl");

    let (tx, rx) = mpsc::unbounded_channel();
    let host = spawn_mock_host(rx, |req| {
        if let WorkflowHostRequest::SpawnAgent { reply, .. } = req {
            let _ = reply.send(Ok(agent_result("y")));
        }
    });
    let _ = run_workflow(params(script, Journal::new(Some(journal_path.clone())), tx));
    drop(host);

    let (tx, rx) = mpsc::unbounded_channel();
    let phases = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let phases_in_host = phases.clone();
    let host = spawn_mock_host(rx, move |req| {
        if let WorkflowHostRequest::Phase { title, replayed } = req {
            phases_in_host.lock().unwrap().push((title, replayed));
        }
    });
    let _ = run_workflow(params(script, Journal::load(journal_path).unwrap(), tx));
    host.join().unwrap();

    let phases = phases.lock().unwrap();
    assert_eq!(
        phases.as_slice(),
        &[("One".into(), true), ("Two".into(), false)]
    );
}

#[test]
fn json_encode_quotes_untrusted_strings() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = run_workflow(params(
        r#"
            let meta = #{ name: "t", description: "d" };
            complete(json_encode("</tag>\nquoted"));
            "#,
        Journal::new(None),
        tx,
    ));
    match outcome {
        WorkflowOutcome::Completed { result } => {
            assert_eq!(result, serde_json::json!("\"</tag>\\nquoted\""));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}
