//! Background workflow runtime.
//!
//! Implemented here: engine integration, four-scope registry, path/persistence
//! primitives, tracker, schema contract, admission/gate, request routing, the
//! typed control-error contract, the run lifecycle orchestrator, the host
//! service, and the child-agent spawn hub. The `workflow` tool lives in
//! `tools::handlers::workflow`; the app-server and TUI surfaces are wired
//! through the protocol crate. Residual gaps are tracked in the COS-1559 TDD
//! ledger rather than in this module comment.

pub(crate) mod admission;
pub(crate) mod assets;
pub(crate) mod gate;
pub(crate) mod host_service;
pub(crate) mod listing;
pub(crate) mod manager;
pub(crate) mod notify;
pub(crate) mod outcome_hub;
pub(crate) mod paths;
pub(crate) mod persist;
pub(crate) mod registry;
pub(crate) mod request_service;
pub(crate) mod run_manager;
pub(crate) mod runtime;
pub(crate) mod schema_contract;
pub(crate) mod store;
/// Hidden harness surface for the integration E2E/failure matrices (D-19).
#[doc(hidden)]
pub mod testing;
pub(crate) mod tracker;

pub use manager::WorkflowControlError;
pub use paths::WorkflowPathError;
pub(crate) use paths::WorkflowPaths;
pub(crate) use request_service::{RequestService, WorkflowRequest};
pub use run_manager::WorkflowManager;
pub use run_manager::WorkflowRunSummary;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::Weak;

use codex_protocol::ThreadId;

use manager::WorkflowLimits;
use run_manager::WorkflowManager as Manager;

/// Session-scoped manager cache keyed by thread id.
static MANAGERS: OnceLock<Mutex<HashMap<ThreadId, Weak<Manager>>>> = OnceLock::new();

/// Return the single workflow manager owned by `thread`.
///
/// The first call resolves `Session::workflow_paths()` and builds a manager with
/// `[workflow]` limits; later calls reuse the `Weak` upgrade so a dropped
/// manager is recreated instead of resurrecting stale run state.
pub async fn manager_for_thread(
    thread: &crate::CodexThread,
) -> Result<Arc<Manager>, WorkflowControlError> {
    manager_for_session(&thread.session).await
}

/// Same cache, reachable from inside a tool invocation where only the
/// `Arc<Session>` is available.
pub(crate) async fn manager_for_session(
    session: &Arc<crate::session::session::Session>,
) -> Result<Arc<Manager>, WorkflowControlError> {
    let key = session.thread_id();
    let cache = MANAGERS.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(manager) = guard.get(&key).and_then(Weak::upgrade) {
            return Ok(manager);
        }
    }

    let paths = session.workflow_paths().await?;
    // Install/upgrade the compiled-in bundled scripts and prompt templates
    // before the registry scans `bundled_dir`, so a fresh session sees the
    // shipped assets without a separate setup step.
    if let Err(error) = assets::ensure_bundled_assets(paths.bundled_dir()) {
        tracing::warn!(
            error = %error,
            bundled_dir = %paths.bundled_dir().display(),
            "workflow bundled asset install failed"
        );
    }
    let limits = WorkflowLimits::from_config(Some(&session.workflow_config().await));
    let state = session
        .services
        .local_agent_runtime
        .thread_manager_state()
        .map_err(|error| WorkflowControlError::Unavailable(error.to_string()))?;
    let anchor = state
        .get_thread(session.thread_id())
        .await
        .map_err(|error| WorkflowControlError::Unavailable(error.to_string()))?;
    let manager = Arc::new(Manager::with_limits(
        paths,
        Arc::new(runtime::EngineRunner::new(anchor)),
        limits,
    ));
    notify::spawn_event_forwarder(Arc::clone(session), Arc::clone(&manager));
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, Arc::downgrade(&manager));
    Ok(manager)
}
