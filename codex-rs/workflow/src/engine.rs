//! Workflow script execution engine.
//!
//! Drives Rhai evaluation, host callbacks, cancellation, and phase-result
//! collection for a single workflow run.

use std::cell::RefCell;
use std::rc::Rc;

use rhai::{Dynamic, EvalAltResult, FnPtr, NativeCallContext, Position};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::host::{AgentOpts, HostError, WorkflowHostRequest};
use crate::journal::{HOST_ERROR_KEY, Journal, JournalError, request_hash};
use crate::run::{PauseKind, WorkflowOutcome};
use crate::{MAX_HOST_CALLS, MAX_PARALLEL, MAX_WORKFLOW_DEPTH};

pub struct WorkflowRunParams {
    pub script: String,
    pub args: serde_json::Value,
    pub journal: Journal,
    pub host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    pub cancel: CancellationToken,
    pub max_ops: u64,
}

impl WorkflowRunParams {
    pub const DEFAULT_MAX_OPS: u64 = 100_000_000;
}

#[derive(Debug, Clone)]
enum ControlToken {
    Complete(serde_json::Value),
    Pause(PauseKind, String),
    Budget(String),
    Cancelled,
    Fatal(String),
}

struct Ctx {
    host_tx: mpsc::UnboundedSender<WorkflowHostRequest>,
    journal: Journal,
    seq: u64,
    workflow_stack: Vec<String>,
}

impl Ctx {
    fn replaying(&self) -> bool {
        self.journal.covers(self.seq)
    }

    fn next_seq(&mut self) -> ScriptResult<u64> {
        Ok(self.reserve_seqs(1)?.start)
    }

    fn reserve_seqs(&mut self, count: usize) -> ScriptResult<std::ops::Range<u64>> {
        let count = u64::try_from(count).map_err(|_| {
            terminated(ControlToken::Fatal(
                "workflow host-call count overflowed".into(),
            ))
        })?;
        let end = self.seq.checked_add(count).ok_or_else(|| {
            terminated(ControlToken::Fatal(
                "workflow host-call count overflowed".into(),
            ))
        })?;
        if end > MAX_HOST_CALLS {
            return Err(terminated(ControlToken::Fatal(format!(
                "workflow exceeded the maximum of {MAX_HOST_CALLS} result-bearing host calls"
            ))));
        }
        let start = self.seq;
        self.seq = end;
        Ok(start..end)
    }

    fn enter_workflow(&mut self, name: &str) -> ScriptResult<()> {
        if self.workflow_stack.iter().any(|entry| entry == name) {
            return Err(runtime_error(format!(
                "workflow recursion detected: {} -> {name}",
                self.workflow_stack.join(" -> ")
            )));
        }
        if self.workflow_stack.len() >= MAX_WORKFLOW_DEPTH {
            return Err(runtime_error(format!(
                "workflow nesting depth exceeded (maximum {MAX_WORKFLOW_DEPTH})"
            )));
        }
        self.workflow_stack.push(name.to_string());
        Ok(())
    }

    fn leave_workflow(&mut self) {
        self.workflow_stack.pop();
    }

    fn scoped_phase(&self, title: &str) -> String {
        if self.workflow_stack.is_empty() {
            title.to_string()
        } else {
            format!("{} / {title}", self.workflow_stack.join(" / "))
        }
    }

    fn record(
        &mut self,
        seq: u64,
        kind: &str,
        hash: String,
        value: serde_json::Value,
    ) -> ScriptResult<()> {
        self.journal
            .record(seq, kind, hash, value)
            .map_err(journal_fatal)
    }
}

type ScriptResult<T> = Result<T, Box<EvalAltResult>>;

enum PendingAgent {
    Replayed(serde_json::Value),
    Live {
        seq: u64,
        hash: String,
        reply_rx: oneshot::Receiver<Result<crate::host::AgentResult, HostError>>,
    },
}

fn drain_parallel_replies(pending: Vec<PendingAgent>) {
    for entry in pending {
        if let PendingAgent::Live { reply_rx, .. } = entry {
            let _ = reply_rx.blocking_recv();
        }
    }
}

pub fn run_workflow(params: WorkflowRunParams) -> WorkflowOutcome {
    let WorkflowRunParams {
        script,
        args,
        journal,
        host_tx,
        cancel,
        max_ops,
    } = params;

    let ctx = Rc::new(RefCell::new(Ctx {
        host_tx,
        journal,
        seq: 0,
        workflow_stack: Vec::new(),
    }));

    let mut engine = rhai::Engine::new();
    engine.set_max_operations(max_ops);
    engine.set_max_call_levels(64);
    engine.set_max_expr_depths(128, 64);
    engine.set_max_string_size(16 * 1024 * 1024);
    engine.set_max_array_size(65_536);
    engine.set_max_map_size(65_536);
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());
    engine.disable_symbol("eval");
    engine.register_fn("timestamp", || -> ScriptResult<()> {
        Err(runtime_error(
            "timestamp() is unavailable: workflow scripts must be deterministic (wall-clock \
             time breaks resume). Pass timestamps in via `args` instead.",
        ))
    });
    engine.register_fn("sleep", |_seconds: i64| -> ScriptResult<()> {
        Err(runtime_error(
            "sleep() is unavailable in workflow scripts — host calls already block until \
             their work finishes.",
        ))
    });
    engine.register_fn("sleep", |_seconds: f64| -> ScriptResult<()> {
        Err(runtime_error(
            "sleep() is unavailable in workflow scripts — host calls already block until \
             their work finishes.",
        ))
    });
    engine.register_fn("exit", || -> ScriptResult<()> {
        Err(runtime_error(
            "exit() is unavailable — end a workflow with complete(value) or pause(kind, msg).",
        ))
    });

    engine.on_progress(move |_ops| {
        if cancel.is_cancelled() {
            Some(Dynamic::from(ControlToken::Cancelled))
        } else {
            None
        }
    });

    register_host_fns(&mut engine, &ctx);

    let ast = match engine.compile(&script) {
        Ok(ast) => ast,
        Err(e) => {
            return WorkflowOutcome::Failed {
                error: format!("script failed to compile: {e}"),
            };
        }
    };

    let mut scope = rhai::Scope::new();
    let args_dyn = match rhai::serde::to_dynamic(&args) {
        Ok(d) => d,
        Err(e) => {
            return WorkflowOutcome::Failed {
                error: format!("invalid workflow args: {e}"),
            };
        }
    };
    scope.push_dynamic("args", args_dyn);

    match engine.eval_ast_with_scope::<Dynamic>(&mut scope, &ast) {
        Ok(value) => WorkflowOutcome::Completed {
            result: dynamic_to_value(value),
        },
        Err(err) => outcome_from_error(*err),
    }
}

fn outcome_from_error(err: EvalAltResult) -> WorkflowOutcome {
    if let Some(token) = find_control_token(&err) {
        return match token {
            ControlToken::Complete(result) => WorkflowOutcome::Completed { result },
            ControlToken::Pause(kind, message) => WorkflowOutcome::Paused { kind, message },
            ControlToken::Budget(message) => WorkflowOutcome::BudgetExceeded { message },
            ControlToken::Cancelled => WorkflowOutcome::Cancelled,
            ControlToken::Fatal(error) => WorkflowOutcome::Failed { error },
        };
    }
    WorkflowOutcome::Failed {
        error: crate::with_rhai_hint(err.to_string()),
    }
}

fn find_control_token(err: &EvalAltResult) -> Option<ControlToken> {
    match err {
        EvalAltResult::ErrorTerminated(token, _) => token.clone().try_cast::<ControlToken>(),
        EvalAltResult::ErrorInFunctionCall(_, _, inner, _) => find_control_token(inner),
        EvalAltResult::ErrorInModule(_, inner, _) => find_control_token(inner),
        _ => None,
    }
}

fn terminated(token: ControlToken) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorTerminated(
        Dynamic::from(token),
        Position::NONE,
    ))
}

fn runtime_error(message: impl Into<String>) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(
        Dynamic::from(message.into()),
        Position::NONE,
    ))
}

fn journal_fatal(error: JournalError) -> Box<EvalAltResult> {
    terminated(ControlToken::Fatal(error.to_string()))
}

fn dynamic_to_value(d: Dynamic) -> serde_json::Value {
    rhai::serde::from_dynamic::<serde_json::Value>(&d).unwrap_or(serde_json::Value::Null)
}

fn value_to_dynamic(v: &serde_json::Value) -> ScriptResult<Dynamic> {
    rhai::serde::to_dynamic(v).map_err(|e| runtime_error(format!("host result conversion: {e}")))
}

fn inline_workflow_call(
    context: &NativeCallContext,
    ctx: &Rc<RefCell<Ctx>>,
    name: &str,
    args: Dynamic,
) -> ScriptResult<Dynamic> {
    let name = name.to_string();
    ctx.borrow_mut().enter_workflow(&name)?;

    let result = (|| {
        let args_value = if args.is_unit() {
            serde_json::Value::Null
        } else {
            rhai::serde::from_dynamic::<serde_json::Value>(&args)
                .map_err(|e| runtime_error(format!("invalid workflow args: {e}")))?
        };
        let payload = serde_json::json!({
            "name": name.clone(),
            "args": args_value,
        });
        let script_value = host_call(
            ctx,
            "resolve_workflow",
            payload,
            |reply| WorkflowHostRequest::ResolveWorkflow {
                name: name.clone(),
                reply,
            },
            serde_json::Value::String,
        )?;
        let script = script_value
            .as_str()
            .ok_or_else(|| runtime_error("workflow resolver returned a non-string script"))?;
        let ast = context
            .engine()
            .compile(script)
            .map_err(|e| runtime_error(format!("workflow '{name}' failed to compile: {e}")))?;
        let args_dyn = value_to_dynamic(&args_value)?;
        let mut scope = rhai::Scope::new();
        scope.push_dynamic("args", args_dyn);

        match context
            .engine()
            .eval_ast_with_scope::<Dynamic>(&mut scope, &ast)
        {
            Ok(value) => Ok(value),
            Err(error) => match find_control_token(&error) {
                Some(ControlToken::Complete(value)) => value_to_dynamic(&value),
                Some(token) => Err(terminated(token)),
                None => Err(error),
            },
        }
    })();

    ctx.borrow_mut().leave_workflow();
    result
}

fn map_to_value(map: rhai::Map) -> ScriptResult<serde_json::Value> {
    rhai::serde::from_dynamic::<serde_json::Value>(&Dynamic::from_map(map))
        .map_err(|e| runtime_error(format!("invalid options map: {e}")))
}

fn replay_spawn_agent(
    journal: &Journal,
    seq: u64,
    payload: &serde_json::Value,
    hash: &str,
) -> Result<Option<serde_json::Value>, JournalError> {
    let normal = journal.replay(seq, "spawn_agent", hash);
    match &normal {
        Ok(Some(_)) => return normal,
        Ok(None) | Err(JournalError::Divergence { .. }) => {}
        Err(_) => return normal,
    }

    let Some(mut legacy_payload) = payload.as_object().cloned() else {
        return normal;
    };
    if legacy_payload.remove("effort").is_none() {
        return normal;
    }
    let legacy_hash = request_hash("spawn_agent", &serde_json::Value::Object(legacy_payload));
    journal.replay(seq, "spawn_agent", &legacy_hash)
}

fn host_call<T>(
    ctx: &Rc<RefCell<Ctx>>,
    kind: &'static str,
    payload: serde_json::Value,
    build: impl FnOnce(oneshot::Sender<Result<T, HostError>>) -> WorkflowHostRequest,
    to_result: impl FnOnce(T) -> serde_json::Value,
) -> ScriptResult<serde_json::Value> {
    let hash = request_hash(kind, &payload);
    let seq = ctx.borrow_mut().next_seq()?;

    let replayed = {
        let ctx = ctx.borrow();
        if kind == "spawn_agent" {
            replay_spawn_agent(&ctx.journal, seq, &payload, &hash)
        } else {
            ctx.journal.replay(seq, kind, &hash)
        }
    };
    match replayed {
        Ok(Some(recorded)) => {
            if let Some(err) = replay_host_error(&recorded) {
                return Err(err);
            }
            return Ok(recorded);
        }
        Ok(None) => {}
        Err(error) => return Err(journal_fatal(error)),
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.borrow()
        .host_tx
        .send(build(reply_tx))
        .map_err(|_| terminated(ControlToken::Fatal("workflow host channel closed".into())))?;

    let reply = reply_rx
        .blocking_recv()
        .map_err(|_| terminated(ControlToken::Fatal("workflow host dropped reply".into())))?;

    let value = match reply {
        Ok(v) => to_result(v),
        Err(HostError::AgentCallQuotaExceeded { requested, maximum }) => {
            return Err(runtime_error(format!(
                "workflow agent-call quota exceeded: requested {requested}, maximum {maximum}"
            )));
        }
        Err(HostError::BudgetExceeded) => {
            return Err(terminated(ControlToken::Budget(
                "workflow agent budget exceeded".into(),
            )));
        }
        Err(HostError::Cancelled) => return Err(terminated(ControlToken::Cancelled)),
        Err(HostError::Unsupported(msg)) => {
            let sentinel = host_error_sentinel(&msg);
            ctx.borrow_mut().record(seq, kind, hash, sentinel)?;
            return Err(runtime_error(msg));
        }
        Err(HostError::Failed(msg)) => {
            let sentinel = host_error_sentinel(&msg);
            ctx.borrow_mut().record(seq, kind, hash, sentinel)?;
            return Err(runtime_error(msg));
        }
    };

    ctx.borrow_mut().record(seq, kind, hash, value.clone())?;
    Ok(value)
}

const HOST_TERMINAL_KEY: &str = "__codex_workflow_parallel_terminal";
const TERMINAL_BUDGET: &str = "budget_exceeded";
const TERMINAL_CANCELLED: &str = "cancelled";
const TERMINAL_DROPPED_REPLY: &str = "dropped_reply";

fn host_error_sentinel(message: &str) -> serde_json::Value {
    serde_json::json!({ HOST_ERROR_KEY: message })
}

fn replay_host_error(recorded: &serde_json::Value) -> Option<Box<rhai::EvalAltResult>> {
    let message = recorded.get(HOST_ERROR_KEY)?.as_str()?;
    Some(runtime_error(message.to_string()))
}

fn host_terminal_sentinel(kind: &str) -> serde_json::Value {
    serde_json::json!({ HOST_TERMINAL_KEY: kind })
}

fn host_terminal_error(kind: &str) -> Box<rhai::EvalAltResult> {
    match kind {
        TERMINAL_BUDGET => terminated(ControlToken::Budget(
            "workflow agent budget exceeded".into(),
        )),
        TERMINAL_CANCELLED => terminated(ControlToken::Cancelled),
        TERMINAL_DROPPED_REPLY => {
            terminated(ControlToken::Fatal("workflow host dropped reply".into()))
        }
        _ => terminated(ControlToken::Fatal(
            "workflow journal contains an unknown terminal marker".into(),
        )),
    }
}

fn is_host_terminal_sentinel(recorded: &serde_json::Value) -> bool {
    recorded.get(HOST_TERMINAL_KEY).is_some()
}

fn host_emit(ctx: &Rc<RefCell<Ctx>>, build: impl FnOnce(bool) -> WorkflowHostRequest) {
    let (replaying, tx) = {
        let ctx = ctx.borrow();
        (ctx.replaying(), ctx.host_tx.clone())
    };
    let _ = tx.send(build(replaying));
}

fn reserve_agent_calls(ctx: &Rc<RefCell<Ctx>>, count: usize) -> ScriptResult<()> {
    if count == 0 {
        return Ok(());
    }
    let count =
        u64::try_from(count).map_err(|_| runtime_error("workflow agent-call count overflowed"))?;
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.borrow()
        .host_tx
        .send(WorkflowHostRequest::ReserveAgentCalls {
            count,
            reply: reply_tx,
        })
        .map_err(|_| terminated(ControlToken::Fatal("workflow host channel closed".into())))?;
    match reply_rx
        .blocking_recv()
        .map_err(|_| terminated(ControlToken::Fatal("workflow host dropped reply".into())))?
    {
        Ok(()) => Ok(()),
        Err(HostError::AgentCallQuotaExceeded { requested, maximum }) => {
            Err(terminated(ControlToken::Budget(format!(
                "workflow agent budget exceeded: requested {requested}, maximum {maximum}"
            ))))
        }
        Err(HostError::Cancelled) => Err(terminated(ControlToken::Cancelled)),
        Err(HostError::BudgetExceeded) => Err(terminated(ControlToken::Budget(
            "workflow agent budget exceeded".into(),
        ))),
        Err(HostError::Unsupported(message) | HostError::Failed(message)) => {
            Err(runtime_error(message))
        }
    }
}

fn release_agent_calls(ctx: &Rc<RefCell<Ctx>>, count: usize) {
    if count == 0 {
        return;
    }
    let Ok(count) = u64::try_from(count) else {
        return;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if ctx
        .borrow()
        .host_tx
        .send(WorkflowHostRequest::ReleaseAgentCalls {
            count,
            reply: reply_tx,
        })
        .is_err()
    {
        return;
    }
    let _ = reply_rx.blocking_recv();
}

fn is_resumable_unjournaled_terminal(err: &EvalAltResult) -> bool {
    matches!(
        find_control_token(err),
        Some(ControlToken::Cancelled | ControlToken::Budget(_))
    )
}

fn spawn_agent_call(ctx: &Rc<RefCell<Ctx>>, opts: AgentOpts) -> ScriptResult<Dynamic> {
    let payload = serde_json::to_value(&opts)
        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
    let hash = request_hash("spawn_agent", &payload);
    let is_live = {
        let ctx = ctx.borrow();
        match replay_spawn_agent(&ctx.journal, ctx.seq, &payload, &hash) {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => return Err(journal_fatal(error)),
        }
    };
    if is_live {
        reserve_agent_calls(ctx, 1)?;
    }
    let value = match host_call(
        ctx,
        "spawn_agent",
        payload,
        |reply| WorkflowHostRequest::SpawnAgent { opts, reply },
        |result| serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
    ) {
        Ok(value) => value,
        Err(err) => {
            if is_live && is_resumable_unjournaled_terminal(&err) {
                release_agent_calls(ctx, 1);
            }
            return Err(err);
        }
    };
    value_to_dynamic(&value)
}

fn agent_opts_from_map(prompt: Option<&str>, map: rhai::Map) -> ScriptResult<AgentOpts> {
    let value = map_to_value(map)?;
    let mut opts: AgentOpts = serde_json::from_value(value)
        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
    if let Some(prompt) = prompt {
        opts.prompt = prompt.to_string();
    }
    if opts.prompt.trim().is_empty() {
        return Err(runtime_error("agent prompt must not be empty"));
    }
    Ok(opts)
}

fn pipeline_impl(
    context: &NativeCallContext,
    items: rhai::Array,
    stages: &[FnPtr],
) -> ScriptResult<rhai::Array> {
    if items.len() > MAX_PARALLEL {
        return Err(runtime_error(format!(
            "pipeline() accepts at most {MAX_PARALLEL} items per call (got {})",
            items.len()
        )));
    }

    let mut results = rhai::Array::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let index = rhai::INT::try_from(index)
            .map_err(|_| runtime_error("pipeline() item index overflowed"))?;
        let original = item.clone();
        let mut previous = item;
        for stage in stages {
            previous = stage
                .call_within_context::<Dynamic>(context, (previous, original.clone(), index))?;
        }
        results.push(previous);
    }
    Ok(results)
}

fn pipeline_from_stage_array(
    context: &NativeCallContext,
    items: rhai::Array,
    stage_values: rhai::Array,
) -> ScriptResult<rhai::Array> {
    let stages = stage_values
        .into_iter()
        .map(|stage| {
            stage
                .try_cast::<FnPtr>()
                .ok_or_else(|| runtime_error("pipeline() stages must be function closures"))
        })
        .collect::<ScriptResult<Vec<_>>>()?;
    pipeline_impl(context, items, &stages)
}

fn register_host_fns(engine: &mut rhai::Engine, ctx: &Rc<RefCell<Ctx>>) {
    let c = ctx.clone();
    engine.register_fn("agent", move |prompt: &str| -> ScriptResult<Dynamic> {
        spawn_agent_call(
            &c,
            AgentOpts {
                prompt: prompt.to_string(),
                ..Default::default()
            },
        )
    });
    let c = ctx.clone();
    engine.register_fn(
        "agent",
        move |prompt: &str, opts: rhai::Map| -> ScriptResult<Dynamic> {
            let opts = agent_opts_from_map(Some(prompt), opts)?;
            spawn_agent_call(&c, opts)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "parallel",
        move |items: rhai::Array| -> ScriptResult<rhai::Array> {
            if items.len() > MAX_PARALLEL {
                return Err(runtime_error(format!(
                    "parallel() accepts at most {MAX_PARALLEL} items per call (got {})",
                    items.len()
                )));
            }
            let mut opts_list = Vec::with_capacity(items.len());
            for item in items {
                let map = item
                    .try_cast::<rhai::Map>()
                    .ok_or_else(|| runtime_error("parallel() items must be option maps"))?;
                opts_list.push(agent_opts_from_map(None, map)?);
            }

            let requests = opts_list
                .into_iter()
                .map(|opts| {
                    let payload = serde_json::to_value(&opts)
                        .map_err(|e| runtime_error(format!("invalid agent options: {e}")))?;
                    let hash = request_hash("spawn_agent", &payload);
                    Ok((opts, payload, hash))
                })
                .collect::<ScriptResult<Vec<_>>>()?;
            let live_count = {
                let ctx = c.borrow();
                let mut seq = ctx.seq;
                let mut live = 0usize;
                for (_, payload, hash) in &requests {
                    match replay_spawn_agent(&ctx.journal, seq, payload, hash) {
                        Ok(Some(_)) => {}
                        Ok(None) => live += 1,
                        Err(error) => return Err(journal_fatal(error)),
                    }
                    seq = seq.checked_add(1).ok_or_else(|| {
                        terminated(ControlToken::Fatal(
                            "workflow host-call count overflowed".into(),
                        ))
                    })?;
                }
                live
            };
            reserve_agent_calls(&c, live_count)?;
            let mut pending = Vec::with_capacity(requests.len());
            for (opts, payload, hash) in requests {
                let seq = c.borrow_mut().next_seq().inspect_err(|_| {
                    drain_parallel_replies(std::mem::take(&mut pending));
                })?;
                match replay_spawn_agent(&c.borrow().journal, seq, &payload, &hash) {
                    Ok(Some(value)) => pending.push(PendingAgent::Replayed(value)),
                    Ok(None) => {
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if c.borrow()
                            .host_tx
                            .send(WorkflowHostRequest::SpawnAgent {
                                opts,
                                reply: reply_tx,
                            })
                            .is_err()
                        {
                            drain_parallel_replies(pending);
                            return Err(terminated(ControlToken::Fatal(
                                "workflow host channel closed".into(),
                            )));
                        }
                        pending.push(PendingAgent::Live {
                            seq,
                            hash,
                            reply_rx,
                        });
                    }
                    Err(error) => {
                        drain_parallel_replies(pending);
                        return Err(journal_fatal(error));
                    }
                }
            }

            let mut resolved: Vec<(Option<(u64, String)>, serde_json::Value)> =
                Vec::with_capacity(pending.len());
            let mut terminal_kind: Option<String> = None;
            let mut terminal_error = None;
            let mut resumable_terminal = false;
            for entry in pending {
                match entry {
                    PendingAgent::Replayed(value) => {
                        if is_host_terminal_sentinel(&value) {
                            let kind = value
                                .get(HOST_TERMINAL_KEY)
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            if kind == TERMINAL_BUDGET || kind == TERMINAL_CANCELLED {
                                terminal_kind.get_or_insert(kind);
                                resumable_terminal = true;
                            } else {
                                terminal_kind.get_or_insert(kind);
                            }
                            continue;
                        }
                        if let Some(error) = replay_host_error(&value) {
                            terminal_error.get_or_insert(error);
                            continue;
                        }
                        resolved.push((None, value));
                    }
                    PendingAgent::Live {
                        seq,
                        hash,
                        reply_rx,
                    } => {
                        let value = match reply_rx.blocking_recv() {
                            Ok(Ok(result)) => {
                                serde_json::to_value(result).unwrap_or(serde_json::Value::Null)
                            }
                            Ok(Err(HostError::BudgetExceeded)) => {
                                terminal_kind.get_or_insert_with(|| TERMINAL_BUDGET.to_string());
                                resumable_terminal = true;
                                host_terminal_sentinel(TERMINAL_BUDGET)
                            }
                            Ok(Err(HostError::Cancelled)) => {
                                terminal_kind.get_or_insert_with(|| TERMINAL_CANCELLED.to_string());
                                resumable_terminal = true;
                                host_terminal_sentinel(TERMINAL_CANCELLED)
                            }
                            Err(_) => {
                                terminal_kind
                                    .get_or_insert_with(|| TERMINAL_DROPPED_REPLY.to_string());
                                host_terminal_sentinel(TERMINAL_DROPPED_REPLY)
                            }
                            Ok(Err(
                                HostError::AgentCallQuotaExceeded { .. }
                                | HostError::Unsupported(_)
                                | HostError::Failed(_),
                            )) => serde_json::Value::Null,
                        };
                        resolved.push((Some((seq, hash)), value));
                    }
                }
            }

            if resumable_terminal {
                release_agent_calls(&c, live_count);
            } else {
                for (live, value) in &resolved {
                    let Some((seq, hash)) = live else {
                        continue;
                    };
                    if let Err(error) =
                        c.borrow_mut()
                            .record(*seq, "spawn_agent", hash.clone(), value.clone())
                    {
                        terminal_error.get_or_insert(error);
                        break;
                    }
                }
            }

            if let Some(kind) = terminal_kind {
                return Err(host_terminal_error(&kind));
            }
            if let Some(error) = terminal_error {
                return Err(error);
            }

            let mut results = rhai::Array::with_capacity(resolved.len());
            for (_, value) in resolved {
                results.push(value_to_dynamic(&value)?);
            }
            Ok(results)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "workflow",
        move |context: NativeCallContext, name: &str, args: Dynamic| -> ScriptResult<Dynamic> {
            inline_workflow_call(&context, &c, name, args)
        },
    );
    let c = ctx.clone();
    engine.register_fn(
        "workflow",
        move |context: NativeCallContext, name: &str| -> ScriptResult<Dynamic> {
            inline_workflow_call(&context, &c, name, Dynamic::UNIT)
        },
    );

    macro_rules! register_pipeline {
        ($($stage:ident),+) => {
            engine.register_fn(
                "pipeline",
                move |context: NativeCallContext, items: rhai::Array, $($stage: FnPtr),+|
                    -> ScriptResult<rhai::Array> {
                    let stages = [$($stage),+];
                    pipeline_impl(&context, items, &stages)
                },
            );
        };
    }
    register_pipeline!(stage1);
    register_pipeline!(stage1, stage2);
    register_pipeline!(stage1, stage2, stage3);
    register_pipeline!(stage1, stage2, stage3, stage4);
    register_pipeline!(stage1, stage2, stage3, stage4, stage5);
    register_pipeline!(stage1, stage2, stage3, stage4, stage5, stage6);
    register_pipeline!(stage1, stage2, stage3, stage4, stage5, stage6, stage7);
    register_pipeline!(
        stage1, stage2, stage3, stage4, stage5, stage6, stage7, stage8
    );
    engine.register_fn(
        "pipeline",
        move |context: NativeCallContext,
              items: rhai::Array,
              stage_values: rhai::Array|
              -> ScriptResult<rhai::Array> {
            pipeline_from_stage_array(&context, items, stage_values)
        },
    );

    let c = ctx.clone();
    engine.register_fn("phase", move |title: &str| {
        let title = c.borrow().scoped_phase(title);
        host_emit(&c, |replayed| WorkflowHostRequest::Phase {
            title,
            replayed,
        });
    });

    let c = ctx.clone();
    engine.register_fn("log", move |message: &str| {
        let message = message.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Log {
            message,
            replayed,
        });
    });
    let c = ctx.clone();
    engine.on_print(move |message| {
        let message = message.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Log {
            message,
            replayed,
        });
    });

    let c = ctx.clone();
    engine.on_debug(move |message, _source, _pos| {
        let message = message.to_string();
        host_emit(&c, |replayed| WorkflowHostRequest::Log {
            message,
            replayed,
        });
    });

    let c = ctx.clone();
    engine.register_fn(
        "telemetry_event",
        move |name: &str, fields: rhai::Map| -> ScriptResult<()> {
            let fields = map_to_value(fields)?;
            let name = name.to_string();
            host_emit(&c, |replayed| WorkflowHostRequest::Telemetry {
                name,
                fields,
                replayed,
            });
            Ok(())
        },
    );

    engine.register_fn("complete", move |value: Dynamic| -> ScriptResult<()> {
        Err(terminated(ControlToken::Complete(dynamic_to_value(value))))
    });
    engine.register_fn("complete", move || -> ScriptResult<()> {
        Err(terminated(ControlToken::Complete(serde_json::Value::Null)))
    });

    engine.register_fn(
        "pause",
        move |kind: &str, message: &str| -> ScriptResult<()> {
            let kind: PauseKind = kind.parse().map_err(|e: String| runtime_error(e))?;
            Err(terminated(ControlToken::Pause(kind, message.to_string())))
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "await_user",
        move |kind: &str, message: &str| -> ScriptResult<()> {
            let parsed: PauseKind = kind.parse().map_err(|e: String| runtime_error(e))?;
            let payload = serde_json::json!({ "kind": kind, "message": message });
            let hash = request_hash("await_user", &payload);
            let seq = c.borrow_mut().next_seq()?;
            let replayed = c.borrow().journal.replay(seq, "await_user", &hash);
            match replayed {
                Ok(Some(_)) => Ok(()),
                Ok(None) => {
                    c.borrow_mut()
                        .record(seq, "await_user", hash, serde_json::Value::Null)?;
                    Err(terminated(ControlToken::Pause(parsed, message.to_string())))
                }
                Err(error) => Err(journal_fatal(error)),
            }
        },
    );

    let c = ctx.clone();
    engine.register_fn("budget", move || -> ScriptResult<Dynamic> {
        let value = host_call(
            &c,
            "budget",
            serde_json::Value::Null,
            |reply| WorkflowHostRequest::BudgetQuery { reply },
            |state| serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
        )?;
        value_to_dynamic(&value)
    });

    let c = ctx.clone();
    engine.register_fn(
        "render_template",
        move |name: &str, vars: rhai::Map| -> ScriptResult<Dynamic> {
            let vars = map_to_value(vars)?;
            let payload = serde_json::json!({ "name": name, "vars": vars });
            let name = name.to_string();
            let value = host_call(
                &c,
                "render_template",
                payload,
                |reply| WorkflowHostRequest::RenderTemplate { name, vars, reply },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "write_scratch_file",
        move |name: &str, content: &str| -> ScriptResult<Dynamic> {
            let payload = serde_json::json!({ "name": name, "content": content });
            let (name, content) = (name.to_string(), content.to_string());
            let value = host_call(
                &c,
                "write_scratch_file",
                payload,
                |reply| WorkflowHostRequest::WriteScratchFile {
                    name,
                    content,
                    reply,
                },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "read_scratch_file",
        move |name: &str| -> ScriptResult<Dynamic> {
            let payload = serde_json::json!({ "name": name });
            let name = name.to_string();
            let value = host_call(
                &c,
                "read_scratch_file",
                payload,
                |reply| WorkflowHostRequest::ReadScratchFile { name, reply },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    let c = ctx.clone();
    engine.register_fn(
        "git_diff_since",
        move |commit: &str| -> ScriptResult<Dynamic> {
            let payload = serde_json::json!({ "commit": commit });
            let commit = commit.to_string();
            let value = host_call(
                &c,
                "git_diff_since",
                payload,
                |reply| WorkflowHostRequest::GitDiffSince { commit, reply },
                serde_json::Value::String,
            )?;
            value_to_dynamic(&value)
        },
    );

    engine.register_fn("fingerprint", |text: &str| -> String {
        crate::journal::request_hash("fingerprint", &serde_json::Value::String(text.to_string()))
    });
    engine.register_fn("json_encode", |value: Dynamic| -> ScriptResult<String> {
        serde_json::to_string(&dynamic_to_value(value))
            .map_err(|error| runtime_error(format!("json encoding failed: {error}")))
    });
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
