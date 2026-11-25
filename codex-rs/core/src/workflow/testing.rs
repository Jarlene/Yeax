//! Hidden harness surface for the integration E2E/failure matrices.
//!
//! `core/tests/workflow_e2e.rs` and `core/tests/workflow_failures.rs` live in a
//! separate crate and cannot reach `pub(crate)` workflow internals. This module
//! exposes only what those tests need: a real engine + host-service run driven
//! by scripted mock children (no model credentials), plus the validation
//! helpers behind the failure matrix. It is `#[doc(hidden)]` and not part of the
//! product API (deviation D-19).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_workflow::Journal;
use codex_workflow::WorkflowOutcome;
use codex_workflow::WorkflowRunParams;
use codex_workflow::host::AgentResult;
use codex_workflow::host::HostError;
use codex_workflow::run_workflow;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::assets;
use super::host_service::AgentSpawner;
use super::host_service::SpawnFuture;
use super::host_service::SpawnRequest;
use super::host_service::WorkflowHostService;
use super::paths::WorkflowPaths;
use super::registry::WorkflowRegistry;
use super::schema_contract::SchemaContract;
use super::store::Store;
use super::tracker::Tracker;

/// One scripted terminal turn for a mock child.
#[derive(Debug, Clone)]
pub struct MockAgentReply {
    pub success: bool,
    pub output: Value,
    pub tokens_used: u64,
}

impl MockAgentReply {
    /// A text-only reply, as a real turn would deliver for a prose prompt.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            success: true,
            output: Value::String(text.into()),
            tokens_used: 1,
        }
    }

    /// A structured reply, used by schema-contract scenarios.
    pub fn json(output: Value) -> Self {
        Self {
            success: true,
            output,
            tokens_used: 1,
        }
    }

    /// A failed child turn.
    pub fn failure() -> Self {
        Self {
            success: false,
            output: Value::Null,
            tokens_used: 0,
        }
    }
}

/// Result of one mock-driven run.
#[derive(Debug)]
pub struct MockRunReport {
    pub completed: bool,
    pub output: Value,
    pub error: Option<String>,
    /// Prompts the engine sent to `agent(...)`, in call order.
    pub prompts: Vec<String>,
    pub run_dir: Option<PathBuf>,
}

struct MockSpawner {
    replies: Mutex<VecDeque<MockAgentReply>>,
    prompts: Mutex<Vec<String>>,
}

impl MockSpawner {
    fn new(replies: Vec<MockAgentReply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<String> {
        lock(&self.prompts).clone()
    }
}

impl AgentSpawner for MockSpawner {
    fn resolve_workflow(&self, name: &str) -> Result<String, HostError> {
        Err(HostError::Failed(format!(
            "workflow `{name}` not found in the mock registry"
        )))
    }

    fn spawn_agent(&self, request: SpawnRequest) -> SpawnFuture {
        lock(&self.prompts).push(request.opts.prompt);
        let reply = lock(&self.replies).pop_front();
        Box::pin(async move {
            let reply = reply.ok_or_else(|| {
                HostError::Failed("mock agent script exhausted before the run finished".to_string())
            })?;
            Ok(AgentResult {
                agent_id: "mock-agent".to_string(),
                success: reply.success,
                output: reply.output,
                cancelled: false,
                tokens_used: reply.tokens_used,
                duration_ms: 0,
            })
        })
    }

    fn git_diff_since(&self, _commit: &str) -> Result<String, HostError> {
        Ok(String::new())
    }
}

/// Run `script` through the real Rhai engine and the real host-service protocol,
/// serving every `agent(...)` call from `replies`. This is the mock-child path
/// E2E-01 uses when no model credentials are available.
pub async fn run_script_with_mock_agents(
    script: &str,
    args: Value,
    replies: Vec<MockAgentReply>,
) -> Result<MockRunReport, String> {
    static E2E_RUN_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = E2E_RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "codex-wf-e2e-{}-{}-{}",
        std::process::id(),
        now_ms(),
        seq
    ));
    let codex_home = root.join("codex-home");
    std::fs::create_dir_all(codex_home.join("workflows")).map_err(|error| error.to_string())?;
    let paths = Arc::new(
        WorkflowPaths::resolve_with_git_root(codex_home, None, "e2e-session", None)
            .map_err(|error| error.to_string())?,
    );
    let registry =
        Arc::new(WorkflowRegistry::new(Arc::clone(&paths)).map_err(|error| error.to_string())?);
    let store = Store::new(paths.session_root().to_path_buf());
    let run_id = "wf_e2e_0001";
    let args_text = args.to_string();
    store
        .create_run(
            run_id,
            "mock-e2e",
            "mock e2e objective",
            script,
            &args_text,
            None,
            now_ms(),
        )
        .map_err(|error| error.to_string())?;
    let tracker = Arc::new(Mutex::new(Tracker::new(
        run_id,
        "mock-e2e",
        "mock e2e objective",
        vec!["Run".to_string()],
        Some(64),
        false,
        now_ms(),
    )));
    let spawner = Arc::new(MockSpawner::new(replies));
    let cancel = CancellationToken::new();
    let host = WorkflowHostService::new(
        run_id.to_string(),
        Arc::clone(&tracker),
        store,
        registry,
        spawner.clone(),
        cancel.clone(),
        4,
    );
    let (host_tx, host_rx) = mpsc::unbounded_channel();
    let serve = tokio::spawn(async move { host.serve(host_rx).await });
    let journal_path = paths
        .run_dir(run_id)
        .ok()
        .map(|dir| dir.join("journal.jsonl"));
    let run_dir = paths.run_dir(run_id).ok();
    let params = WorkflowRunParams {
        script: script.to_string(),
        args,
        journal: Journal::new(journal_path),
        host_tx,
        cancel,
        max_ops: WorkflowRunParams::DEFAULT_MAX_OPS,
    };
    let outcome = tokio::task::spawn_blocking(move || run_workflow(params))
        .await
        .map_err(|error| format!("engine task failed: {error}"))?;
    serve.abort();
    let (completed, output, error) = match outcome {
        WorkflowOutcome::Completed { result } => (true, result, None),
        WorkflowOutcome::Paused { kind, message } => (
            false,
            Value::Null,
            Some(format!("paused ({kind:?}): {message}")),
        ),
        WorkflowOutcome::BudgetExceeded { message } => (
            false,
            Value::Null,
            Some(format!("budget exceeded: {message}")),
        ),
        WorkflowOutcome::Cancelled => (false, Value::Null, Some("cancelled".to_string())),
        WorkflowOutcome::Failed { error } => (false, Value::Null, Some(error)),
    };
    Ok(MockRunReport {
        completed,
        output,
        error,
        prompts: spawner.prompts(),
        run_dir,
    })
}

/// Compiled-in builtin script by workflow name.
pub fn builtin_script(name: &str) -> Option<&'static str> {
    assets::builtin_scripts()
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.content)
}

/// Names of every compiled-in builtin workflow.
pub fn builtin_names() -> Vec<&'static str> {
    assets::builtin_scripts()
        .iter()
        .map(|asset| asset.name)
        .collect()
}

/// Prompt template keys shipped with the runtime.
pub fn bundled_template_keys() -> Vec<&'static str> {
    assets::template_keys().to_vec()
}

/// Engine-level validation (meta + run) used by F1..F3.
pub fn validate_script(script: &str) -> Result<(), String> {
    codex_workflow::validate_script(script, None)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Registry script-size ceiling used by F4.
pub fn max_script_bytes() -> usize {
    super::registry::MAX_WORKFLOW_SOURCE_BYTES
}

/// Output-schema contract parsing used by F5.
pub fn validate_output_schema(schema: Value) -> Result<(), String> {
    SchemaContract::parse(schema)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Validate one candidate answer against an output schema (F6/F17/F18).
pub fn validate_schema_text(schema: Value, text: &str) -> Result<Value, String> {
    let contract = SchemaContract::parse(schema).map_err(|error| error.to_string())?;
    contract
        .validate_text(text)
        .map_err(|error| error.to_string())
}

/// Path-segment rule used by F10 (scratch names).
pub fn is_valid_segment(value: &str) -> bool {
    super::paths::validate_segment(value).is_ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
