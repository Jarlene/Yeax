//! In-tree engine runner for [`super::run_manager::WorkflowManager`].
//!
//! The lifecycle manager is deliberately independent of the engine. This module
//! supplies the production runner: it opens a host channel, serves the non-agent
//! host verbs through [`super::host_service::WorkflowHostService`], and runs the
//! Rhai engine on a blocking thread.
//!
//! Child-agent spawning is the one host verb that needs the session's agent
//! runtime. [`Spawner`] creates a host-owned child on the anchor thread
//! (`create_host_child`), delivers the prompt with an explicit
//! `AgentControl::send(TriggerTurn)`, and waits for the run hub to observe the
//! terminal turn.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::MultiAgentVersion;
use codex_workflow::host::AgentResult;
use codex_workflow::host::HostError;
use codex_workflow::{Journal, WorkflowOutcome, WorkflowRunParams, run_workflow};
use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::AgentInput;
use crate::AgentTarget;
use crate::CodexThread;
use crate::HostThreadSpawnReservation;
use crate::SendRequest;
use crate::TurnStartOptions;
use crate::agent::HostChildExtras;
use crate::agent::api::AgentTurnOutcome;
use crate::agent::create_host_child;
use crate::agent::host::AgentRuntimeOwnership;
use crate::agent::host::AgentTurnAdmission;
use crate::agent::host::AgentTurnOutcomeReporter;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::thread_spawn::ChildSpawnTemplate;
use crate::thread_spawn::ThreadCreateRequest;
use crate::thread_spawn::capture_template_from_thread;
use crate::thread_spawn::prepare_detached_in_scope;

use super::admission::WorkflowRunAdmission;
use super::gate::WorkflowRunGate;
use super::host_service::AgentSpawner;
use super::host_service::STOP_DRAIN_TIMEOUT;
use super::host_service::SpawnFuture;
use super::host_service::SpawnRequest;
use super::host_service::WorkflowHostService;
use super::host_service::git_diff_since;
use super::manager::TaskNames;
use super::outcome_hub::PublishOutcome;
use super::outcome_hub::TurnOutcome;
use super::outcome_hub::WorkflowOutcomeHub;
use super::registry::WorkflowRegistry;
use super::run_manager::RunContext;
use super::run_manager::RunFuture;
use super::run_manager::WorkflowRunRunner;
use super::schema_contract::MAX_SCHEMA_ATTEMPTS;
use super::schema_contract::SchemaContract;
use super::store::Store;

/// Upper bound for one child turn. The run-level cancellation path is the
/// normal stop; this only keeps a wedged child from pinning the run forever.
const AGENT_TURN_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// Production runner used by `manager_for_thread`.
pub(crate) struct EngineRunner {
    anchor: Arc<CodexThread>,
}

impl EngineRunner {
    pub(crate) fn new(anchor: Arc<CodexThread>) -> Self {
        Self { anchor }
    }
}

impl WorkflowRunRunner for EngineRunner {
    fn start(&self, context: RunContext) -> RunFuture {
        let anchor = Arc::clone(&self.anchor);
        Box::pin(async move {
            let registry = match WorkflowRegistry::new(Arc::clone(&context.paths)) {
                Ok(registry) => Arc::new(registry),
                Err(error) => {
                    return WorkflowOutcome::Failed {
                        error: format!("workflow registry unavailable: {error}"),
                    };
                }
            };
            let store = Store::new(context.paths.session_root().to_path_buf());
            let cancel = context.cancel.clone();
            let hub = Arc::new(WorkflowOutcomeHub::with_registry(Arc::clone(
                &anchor
                    .session
                    .services
                    .local_agent_runtime
                    .workflow_children,
            )));
            let admission = Arc::new(WorkflowRunAdmission::new(
                Some(usize::from(context.max_concurrent_agents.max(1))),
                cancel.clone(),
            ));
            let gate = Arc::new(WorkflowRunGate::with_capacity(
                admission.capacity(),
                cancel.clone(),
            ));
            tracing::debug!(
                admission_capacity = admission.capacity(),
                admission_available = admission.available(),
                cancelled = admission.cancel_token().is_cancelled(),
                gate_capacity = gate.capacity(),
                gate_available = gate.available(),
                "workflow run started"
            );
            let template = match capture_template_from_thread(
                &anchor,
                &format!("workflow-{}", context.run_id),
                ThreadCreateRequest {
                    task_name: "workflow".to_string(),
                    fork_mode: None,
                    agent_type: None,
                    model: None,
                    reasoning_effort: None,
                },
            )
            .await
            {
                Ok(template) => template,
                Err(error) => {
                    return WorkflowOutcome::Failed {
                        error: format!("workflow spawn template unavailable: {error}"),
                    };
                }
            };
            let spawner = Arc::new(Spawner {
                registry: Arc::clone(&registry),
                anchor: Arc::clone(&anchor),
                template,
                hub: Arc::clone(&hub),
                admission: Arc::clone(&admission),
                gate: Arc::clone(&gate),
                cancel: cancel.clone(),
                names: Arc::new(Mutex::new(HashMap::new())),
                started: Arc::new(Mutex::new(HashMap::new())),
            });
            let (host_tx, host_rx) = mpsc::unbounded_channel();
            let host = WorkflowHostService::new(
                context.run_id.clone(),
                Arc::clone(&context.tracker),
                store,
                registry,
                spawner,
                cancel.clone(),
                context.max_concurrent_agents,
            );
            let serve = tokio::spawn(async move { host.serve(host_rx).await });
            let drain = spawn_cancel_drain(Arc::clone(&anchor), Arc::clone(&hub), cancel.clone());

            let journal_path = context
                .paths
                .run_dir(&context.run_id)
                .map(|dir| dir.join("journal.jsonl"))
                .ok();
            let params = WorkflowRunParams {
                script: context.script,
                args: context.args,
                journal: Journal::new(journal_path),
                host_tx,
                cancel,
                max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
            };
            let outcome = match tokio::task::spawn_blocking(move || run_workflow(params)).await {
                Ok(outcome) => outcome,
                Err(error) => WorkflowOutcome::Failed {
                    error: format!("workflow engine task failed: {error}"),
                },
            };
            serve.abort();
            drain.abort();
            hub.clear_run(&context.run_id);
            outcome
        })
    }
}

/// On stop: interrupt live children, drain their terminals, then release the
/// registry. A timeout keeps tombstones so late terminals never reach the
/// parent mailbox (design 02 §4.3.4).
fn spawn_cancel_drain(
    anchor: Arc<CodexThread>,
    hub: Arc<WorkflowOutcomeHub>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        cancel.cancelled().await;
        let registry = Arc::clone(
            &anchor
                .session
                .services
                .local_agent_runtime
                .workflow_children,
        );
        let ids: Vec<String> = registry
            .active_ids()
            .into_iter()
            .map(|thread_id| thread_id.to_string())
            .collect();
        tracing::debug!(
            active = registry.active_len(),
            finalized = registry.finalized_len(),
            "workflow stop: draining registered children"
        );
        for id in &ids {
            if let Ok(thread_id) = ThreadId::from_string(id) {
                let _ = anchor
                    .session
                    .services
                    .agent_control
                    .interrupt(
                        anchor.session.thread_id,
                        AgentTarget::Id(thread_id),
                        MultiAgentVersion::V2,
                    )
                    .await;
            }
        }
        hub.begin_drain(&ids);
        if hub.wait_for_drain(&ids, STOP_DRAIN_TIMEOUT).await {
            hub.finalize_all(&ids);
        }
        for id in &ids {
            tracing::debug!(
                thread_id = %id,
                has_outcome = hub.outcome(id).is_some(),
                "workflow child drained"
            );
        }
    })
}

struct Spawner {
    registry: Arc<WorkflowRegistry>,
    anchor: Arc<CodexThread>,
    template: ChildSpawnTemplate,
    hub: Arc<WorkflowOutcomeHub>,
    admission: Arc<WorkflowRunAdmission>,
    gate: Arc<WorkflowRunGate>,
    cancel: tokio_util::sync::CancellationToken,
    names: Arc<Mutex<HashMap<String, TaskNames>>>,
    started: Arc<Mutex<HashMap<ThreadId, Instant>>>,
}

impl AgentSpawner for Spawner {
    fn resolve_workflow(&self, name: &str) -> Result<String, HostError> {
        self.registry
            .resolve_by_name(name)
            .map(|workflow| workflow.script)
            .map_err(|error| HostError::Failed(error.to_string()))
    }

    fn spawn_agent(&self, request: SpawnRequest) -> SpawnFuture {
        let anchor = Arc::clone(&self.anchor);
        let template = self.template.clone();
        let hub = Arc::clone(&self.hub);
        let admission = Arc::clone(&self.admission);
        let gate = Arc::clone(&self.gate);
        let cancel = self.cancel.clone();
        let names = Arc::clone(&self.names);
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(HostError::Cancelled);
            }
            let SpawnRequest {
                run_id,
                phase,
                opts,
            } = request;
            let schema = match opts.output_schema.as_ref() {
                Some(schema) => Some(
                    SchemaContract::parse(schema.clone())
                        .map_err(|error| HostError::Failed(error.to_string()))?,
                ),
                None => None,
            };
            let prompt = match &schema {
                Some(contract) => format!("{}\n\n{}", opts.prompt, contract.prompt_suffix()),
                None => opts.prompt.clone(),
            };
            let _gate = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(HostError::Cancelled),
                permit = enter_gate(&gate) => permit.map_err(|error| HostError::Failed(error.to_string()))?,
            };
            let label = opts
                .label
                .clone()
                .or_else(|| opts.phase.clone())
                .or(phase)
                .unwrap_or_else(|| "agent".to_string());
            let task_name = {
                let mut names = names.lock().unwrap_or_else(PoisonError::into_inner);
                names.entry(run_id.clone()).or_default().next(&label)
            };
            let scope = workflow_scope(&run_id);
            let (mut settings, agent_path) =
                prepare_detached_in_scope(&template, &scope, &task_name)
                    .map_err(|error| HostError::Failed(error.to_string()))?;
            // A script-level model wins; otherwise the parent template's
            // effective model (frozen at run start) is inherited.
            let model = opts
                .model
                .clone()
                .or_else(|| template.model().map(str::to_string));
            if let Some(model) = model {
                settings.config.model = Some(model);
            }
            tracing::debug!(
                run_id = %run_id,
                agent = ?agent_path,
                model = ?settings.config.model,
                role = template.role_name().unwrap_or("(default)"),
                "workflow child prepared"
            );
            let resume_config = settings.config.clone();
            let thread_id = anchor
                .session
                .services
                .local_agent_runtime
                .generate_thread_id();
            let admission_provider: Arc<dyn AgentTurnAdmission> = admission.clone();
            let reservation = HostThreadSpawnReservation {
                thread_id,
                control: Arc::clone(&anchor.session.services.agent_control),
                turn_admission: Some(admission_provider),
                usage_accounting: None,
            };
            let extras = HostChildExtras {
                ownership: AgentRuntimeOwnership::Host,
                outcome_reporter: Some(Arc::new(SpawnOutcomeReporter {
                    hub: Arc::clone(&hub),
                    run_id: run_id.clone(),
                    thread_id,
                    cancel: cancel.clone(),
                })),
                workflow_run: Some(run_id.clone()),
            };
            hub.register(&run_id, &thread_id.to_string());
            let mut spawned = create_host_child(&anchor, settings, agent_path, reservation, extras)
                .await
                .map_err(|error| HostError::Failed(error.to_string()))?;
            started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(thread_id, Instant::now());

            let send = anchor.session.services.agent_control.send(SendRequest {
                caller: anchor.session.thread_id,
                target: AgentTarget::Id(thread_id),
                resume_config: resume_config.clone(),
                input: AgentInput::Message {
                    message: AgentMessage::Plaintext(prompt),
                    mode: MessageDeliveryMode::TriggerTurn,
                },
                start_options: TurnStartOptions::default(),
            });
            let send_result = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    drop(spawned.registration.take());
                    interrupt_child(&anchor, thread_id).await;
                    hub.finalize(&thread_id.to_string());
                    return Err(HostError::Cancelled);
                }
                result = send => result,
            };
            match send_result {
                Ok(_) => {
                    if let Some(guard) = spawned.registration.take() {
                        guard.disarm();
                    }
                }
                Err(error) => {
                    drop(spawned.registration.take());
                    interrupt_child(&anchor, thread_id).await;
                    hub.finalize(&thread_id.to_string());
                    return Err(HostError::Failed(format!(
                        "workflow child send failed: {error}"
                    )));
                }
            }

            let thread_key = thread_id.to_string();
            let attempt = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                attempt = hub.wait_for_attempt(&thread_key, 0, AGENT_TURN_TIMEOUT) => attempt,
            };
            let Some(mut attempt) = attempt else {
                hub.finalize(&thread_key);
                if cancel.is_cancelled() {
                    return Err(HostError::Cancelled);
                }
                return Err(HostError::Failed(
                    "workflow child produced no terminal turn".to_string(),
                ));
            };
            // Output-schema contract: one repair turn when the first answer is
            // not valid JSON for the script's schema (design 02 section 5.4).
            let mut schema_attempts = 1u8;
            while let Some(contract) = &schema {
                let text = value_text(&attempt.outcome.output);
                match contract.validate_text(&text) {
                    Ok(validated) => {
                        attempt.outcome.output = validated;
                        break;
                    }
                    Err(error) => {
                        if schema_attempts >= MAX_SCHEMA_ATTEMPTS {
                            hub.finalize(&thread_key);
                            return Err(HostError::Failed(format!(
                                "workflow child {thread_id} output failed schema validation: {error}"
                            )));
                        }
                        schema_attempts += 1;
                        hub.note_attempt(&thread_key);
                        tracing::debug!(
                            thread_id = %thread_id,
                            attempts = hub.attempts(&thread_key),
                            "workflow child output failed schema; retrying"
                        );
                        let retry = anchor.session.services.agent_control.send(SendRequest {
                            caller: anchor.session.thread_id,
                            target: AgentTarget::Id(thread_id),
                            resume_config: resume_config.clone(),
                            input: AgentInput::Message {
                                message: AgentMessage::Plaintext(
                                    contract.retry_prompt(&[error.to_string()]),
                                ),
                                mode: MessageDeliveryMode::TriggerTurn,
                            },
                            start_options: TurnStartOptions::default(),
                        });
                        let send_result = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                hub.finalize(&thread_key);
                                interrupt_child(&anchor, thread_id).await;
                                return Err(HostError::Cancelled);
                            }
                            result = retry => result,
                        };
                        if let Err(error) = send_result {
                            hub.finalize(&thread_key);
                            interrupt_child(&anchor, thread_id).await;
                            return Err(HostError::Failed(format!(
                                "workflow child schema retry failed: {error}"
                            )));
                        }
                        let after_seq = attempt.seq;
                        let next = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => None,
                            attempt = hub.wait_for_attempt(&thread_key, after_seq, AGENT_TURN_TIMEOUT) => attempt,
                        };
                        let Some(next) = next else {
                            hub.finalize(&thread_key);
                            if cancel.is_cancelled() {
                                return Err(HostError::Cancelled);
                            }
                            return Err(HostError::Failed(
                                "workflow child produced no schema retry turn".to_string(),
                            ));
                        };
                        attempt = next;
                    }
                }
            }
            hub.finalize(&thread_key);
            let duration_ms = started
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&thread_id)
                .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            let outcome = attempt.outcome;
            if !outcome.success && !outcome.cancelled {
                return Err(HostError::Failed(format!(
                    "workflow child {thread_id} failed"
                )));
            }
            Ok(AgentResult {
                agent_id: thread_id.to_string(),
                success: outcome.success,
                output: outcome.output,
                cancelled: outcome.cancelled,
                tokens_used: outcome.tokens_used,
                duration_ms,
            })
        })
    }

    fn git_diff_since(&self, commit: &str) -> Result<String, HostError> {
        let cwd = std::env::current_dir()
            .map_err(|error| HostError::Failed(format!("git diff cwd: {error}")))?;
        git_diff_since(&cwd, commit)
    }
}

/// `wf_<uuid7>` → `workflow_<first 8 hex chars>`.
fn workflow_scope(run_id: &str) -> String {
    let suffix = run_id.strip_prefix("wf_").unwrap_or(run_id);
    let short: String = suffix
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(8)
        .collect();
    if short.is_empty() {
        "workflow_run".to_string()
    } else {
        format!("workflow_{short}")
    }
}

/// Prefer the non-blocking gate path; only queue when every slot is busy.
async fn enter_gate(
    gate: &WorkflowRunGate,
) -> Result<super::gate::GatePermit, super::gate::GateError> {
    match gate.try_enter() {
        Ok(permit) => Ok(permit),
        Err(super::gate::GateError::AgentLimitReached { .. }) => gate.enter().await,
        Err(error) => Err(error),
    }
}

/// Raw text of a child's terminal output for schema revalidation.
fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

async fn interrupt_child(anchor: &CodexThread, thread_id: ThreadId) {
    let _ = anchor
        .session
        .services
        .agent_control
        .interrupt(
            anchor.session.thread_id,
            AgentTarget::Id(thread_id),
            MultiAgentVersion::V2,
        )
        .await;
}

/// Installed on every workflow child; forwards terminals to the run hub.
struct SpawnOutcomeReporter {
    hub: Arc<WorkflowOutcomeHub>,
    run_id: String,
    thread_id: ThreadId,
    cancel: tokio_util::sync::CancellationToken,
}

impl AgentTurnOutcomeReporter for SpawnOutcomeReporter {
    fn report(&self, outcome: AgentTurnOutcome) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if outcome.thread_id != self.thread_id {
                return;
            }
            let terminal = turn_outcome(&outcome.status, self.cancel.is_cancelled());
            let published = self.hub.publish(
                &self.thread_id.to_string(),
                stable_turn_id(&outcome.turn_id),
                terminal,
            );
            if matches!(published, PublishOutcome::Rejected) {
                tracing::warn!(
                    run_id = self.run_id,
                    thread_id = %self.thread_id,
                    "workflow terminal arrived after finalization"
                );
            }
        })
    }
}

fn stable_turn_id(turn_id: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    turn_id.hash(&mut hasher);
    hasher.finish()
}

fn turn_outcome(status: &AgentStatus, cancelled: bool) -> TurnOutcome {
    match status {
        AgentStatus::Completed(output) => TurnOutcome {
            success: true,
            output: completed_output(output),
            cancelled: false,
            tokens_used: 0,
        },
        AgentStatus::Interrupted | AgentStatus::Shutdown => TurnOutcome {
            success: false,
            output: serde_json::Value::Null,
            cancelled,
            tokens_used: 0,
        },
        _ => TurnOutcome {
            success: false,
            output: serde_json::Value::Null,
            cancelled: false,
            tokens_used: 0,
        },
    }
}

fn completed_output(output: &Option<String>) -> serde_json::Value {
    match output {
        Some(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.clone()))
        }
        None => serde_json::Value::Null,
    }
}
