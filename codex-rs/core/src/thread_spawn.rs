//! Shared native MAv2 child preparation and spawning.
//!
//! Callers supply captured step settings; the configured agent backend owns child lifetimes.

use crate::agent::api::AgentInput;
use crate::agent::api::SpawnRequest;
use crate::agent::child_config::SpawnConfigOptions;
use crate::agent::child_config::SpawnConfigVersion;
use crate::agent::child_config::prepare_agent_spawn_config;
use crate::agent::helper_daybreak::turn::explicit_program_for_spawn;
use crate::agent::next_thread_spawn_depth;
use crate::agent::types::AgentMessage;
use crate::agent::types::LiveAgent;
use crate::agent::types::MessageDeliveryMode;
use crate::agent::types::SpawnAgentForkMode;
use crate::agent::types::SpawnAgentOptions;
use crate::agent::types::SpawnProgramSource;
use crate::codex_thread::CodexThread;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::session::multi_agents::resolve_usage_hints;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::AgentPath;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tool_call_id::ToolCallId;

/// Child creation options; omitted overrides retain native MAv2 inheritance and role rules.
pub struct ThreadCreateRequest {
    pub task_name: String,
    /// `None` starts fresh; `FullHistory` uses native fork preparation.
    pub fork_mode: Option<SpawnAgentForkMode>,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<String>,
}

pub(crate) struct ThreadSpawnRequest {
    pub task_name: String,
    /// Explicit encoding: hosts normally provide plaintext; native tools can forward ciphertext.
    pub message: AgentMessage,
    /// `None` starts fresh; `FullHistory` uses native fork preparation.
    pub fork_mode: Option<SpawnAgentForkMode>,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub service_tier: Option<String>,
}

impl ThreadSpawnRequest {
    pub(crate) fn into_parts(
        self,
    ) -> Result<(ThreadCreateRequest, AgentMessage), ThreadSpawnError> {
        let message_text = match &self.message {
            AgentMessage::Plaintext(text) | AgentMessage::Encrypted(text) => text,
        };
        if message_text.trim().is_empty() {
            return Err(ThreadSpawnError::Preparation(
                "Empty message can't be sent to an agent".to_string(),
            ));
        }
        Ok((
            ThreadCreateRequest {
                task_name: self.task_name,
                fork_mode: self.fork_mode,
                agent_type: self.agent_type,
                model: self.model,
                reasoning_effort: self.reasoning_effort,
                service_tier: self.service_tier,
            },
            self.message,
        ))
    }
}

/// Whether a child's first turn stays attached to the invoking turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ChildTurnAnchor {
    /// Tool-spawned children attribute their work to the invoking turn.
    ParentTurn,
    /// Workflow runs must outlive the parent turn (design 05 ADR-006).
    Detached,
}

#[derive(Clone)]
pub(crate) struct ChildSpawnSettings {
    pub(crate) config: Config,
    pub(crate) source: SessionSource,
    pub(crate) options: SpawnAgentOptions,
}

/// Parent settings frozen once so a workflow run can start children later.
///
/// Capture happens while a step (or a live thread) is available; every child of
/// the run then reuses the same config, source depth, and overrides. The
/// captured path excludes the per-child name, which `prepare_detached` appends.
#[derive(Clone)]
pub struct ChildSpawnTemplate {
    settings: ChildSpawnSettings,
    agent_path: AgentPath,
    model: Option<String>,
    role_name: Option<String>,
}

impl ChildSpawnTemplate {
    #[allow(dead_code)] // D-17: exercised by thread_spawn_tests.
    pub fn config(&self) -> &Config {
        &self.settings.config
    }

    #[allow(dead_code)] // D-17: exercised by thread_spawn_tests.
    pub fn agent_path(&self) -> &AgentPath {
        &self.agent_path
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn role_name(&self) -> Option<&str> {
        self.role_name.as_deref()
    }
}

/// Child runtime and its effective configuration.
pub struct ThreadSpawnResult {
    pub agent: LiveAgent,
    pub config: ThreadConfigSnapshot,
    pub task_name: AgentPath,
}

/// Separates preparation errors from backend failures to preserve native tool error reporting.
#[derive(Debug, thiserror::Error)]
pub enum ThreadSpawnError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Preparation(String),
    #[error(transparent)]
    Backend(#[from] CodexErr),
}

pub(crate) async fn spawn(
    session: &Session,
    step_context: &StepContext,
    call_id: &ToolCallId,
    request: ThreadSpawnRequest,
) -> Result<ThreadSpawnResult, ThreadSpawnError> {
    let (request, message) = request.into_parts()?;
    let (settings, task_name) = prepare(session, step_context, call_id, request).await?;
    let request = SpawnRequest {
        caller: session.thread_id,
        config: settings.config,
        input: AgentInput::Message {
            message,
            mode: MessageDeliveryMode::TriggerTurn,
            source_tool_call_id: session
                .services
                .rollout_thread_trace
                .is_enabled()
                .then(|| call_id.clone()),
        },
        source: settings.source,
        options: settings.options,
    };
    let (agent, config) = session.services.agent_control.spawn(request).await?;
    step_context
        .turn
        .session_telemetry
        .record_multi_agent_spawned(
            session.session_id(),
            &step_context.turn.sub_id,
            agent.thread_id,
        );
    Ok(ThreadSpawnResult {
        agent,
        config,
        task_name,
    })
}

/// Prepare the native child settings without choosing its coordination backend.
///
/// Single-segment callers (tools) keep today's behavior: the child name is
/// appended to the invoking agent path.
pub(crate) async fn prepare(
    session: &Session,
    step_context: &StepContext,
    call_id: &str,
    request: ThreadCreateRequest,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    let agent_path = step_context
        .turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root)
        .join(&request.task_name)
        .map_err(ThreadSpawnError::Preparation)?;
    prepare_under_path(session, step_context, call_id, request, agent_path).await
}

/// Prepare a child under an already resolved path.
///
/// Workflow runs need `/root/workflow_<run8>/<task>`, which `join` cannot
/// express because it validates a single name segment, so the caller supplies
/// the complete path.
pub(crate) async fn prepare_under_path(
    session: &Session,
    step_context: &StepContext,
    call_id: &str,
    request: ThreadCreateRequest,
    agent_path: AgentPath,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    resolve_settings(
        session,
        step_context,
        call_id,
        request,
        agent_path,
        ChildTurnAnchor::ParentTurn,
    )
    .await
    .map(|(settings, agent_path, _model, _role_name)| (settings, agent_path))
}

/// Freeze the invoking step's settings for later detached children.
pub(crate) async fn capture_template_from_step(
    session: &Session,
    step_context: &StepContext,
    call_id: &str,
    request: ThreadCreateRequest,
) -> Result<ChildSpawnTemplate, ThreadSpawnError> {
    let agent_path = step_context
        .turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let (settings, agent_path, model, role_name) = resolve_settings(
        session,
        step_context,
        call_id,
        request,
        agent_path,
        // Workflow runs outlive the invoking turn (design 05 ADR-006), so the
        // template is captured detached from the start.
        ChildTurnAnchor::Detached,
    )
    .await?;
    Ok(ChildSpawnTemplate {
        settings,
        agent_path,
        model,
        role_name,
    })
}

/// Capture a template from a live thread when no step is available.
///
/// TUI and app-server launches have no invoking step, so the thread's default
/// turn is captured first. That turn is never executed; only its settings,
/// permissions, model, and agent path feed the template.
pub(crate) async fn capture_template_from_thread(
    thread: &CodexThread,
    call_id: &str,
    request: ThreadCreateRequest,
) -> Result<ChildSpawnTemplate, ThreadSpawnError> {
    let session = &thread.session;
    let step = session
        .capture_step_context(
            session.new_default_turn().await,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .map_err(|error| ThreadSpawnError::Preparation(error.to_string()))?;
    capture_template_from_step(session, &step, call_id, request).await
}

/// Start a workflow child from a captured template.
///
/// `parent_turn_id`, `root_turn_id`, and `turn_trigger` are cleared so the run
/// survives the parent turn ending, while the session source, agent path, and
/// rollout stay inherited (design 05 ADR-006).
#[cfg(test)]
pub(crate) fn prepare_detached(
    template: &ChildSpawnTemplate,
    task_name: &str,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    retarget_detached(template, template.agent_path.clone(), task_name)
}

/// Start a workflow child under an extra scope segment.
///
/// Workflow runs use `/root/workflow_<run8>/<task>`; each segment is joined
/// separately so `AgentPath` validates both.
pub(crate) fn prepare_detached_in_scope(
    template: &ChildSpawnTemplate,
    scope: &str,
    task_name: &str,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    let scope_path = template
        .agent_path
        .join(scope)
        .map_err(ThreadSpawnError::Preparation)?;
    retarget_detached(template, scope_path, task_name)
}

fn retarget_detached(
    template: &ChildSpawnTemplate,
    parent_path: AgentPath,
    task_name: &str,
) -> Result<(ChildSpawnSettings, AgentPath), ThreadSpawnError> {
    let agent_path = parent_path
        .join(task_name)
        .map_err(ThreadSpawnError::Preparation)?;
    let mut settings = template.settings.clone();
    settings.source = retarget_source(&settings.source, agent_path.clone());
    settings.options.parent_turn_id = None;
    settings.options.root_turn_id = None;
    settings.options.turn_trigger = None;
    Ok((settings, agent_path))
}

/// Rewrite only the agent path of an inherited thread-spawn source.
fn retarget_source(source: &SessionSource, agent_path: AgentPath) -> SessionSource {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_nickname,
            agent_role,
            agent_path: _,
        }) => SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: *parent_thread_id,
            depth: *depth,
            agent_path: Some(agent_path),
            agent_nickname: agent_nickname.clone(),
            agent_role: agent_role.clone(),
        }),
        other => other.clone(),
    }
}

/// Shared resolution; returns the settings, path, effective model, and role.
#[allow(clippy::too_many_arguments)]
async fn resolve_settings(
    session: &Session,
    step_context: &StepContext,
    call_id: &str,
    request: ThreadCreateRequest,
    task_name: AgentPath,
    anchor: ChildTurnAnchor,
) -> Result<
    (
        ChildSpawnSettings,
        AgentPath,
        Option<String>,
        Option<String>,
    ),
    ThreadSpawnError,
> {
    let turn = &step_context.turn;
    let ThreadCreateRequest {
        task_name: _,
        fork_mode,
        agent_type,
        model,
        reasoning_effort,
        service_tier,
    } = request;
    let role_name = agent_type
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty());
    let prepared = prepare_agent_spawn_config(
        session,
        step_context,
        SpawnConfigOptions {
            version: SpawnConfigVersion::V2,
            full_history_fork: matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory)),
            role_name,
            model: model.as_deref(),
            reasoning_effort,
            service_tier,
        },
    )
    .await
    .map_err(ThreadSpawnError::Preparation)?;
    let config = prepared.config;
    let effective_role = prepared.role_name.clone();
    let spawn_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: next_thread_spawn_depth(&turn.session_source),
        agent_path: Some(task_name.clone()),
        agent_nickname: None,
        agent_role: prepared.role_name,
    });
    let multi_agent_v2_usage_hints = if matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory))
        && turn.multi_agent_version == MultiAgentVersion::V2
    {
        let child_model_info = match config.model.as_deref() {
            Some(model) if model != turn.model_info().slug => Some(
                session
                    .services
                    .models_manager
                    .get_model_info(model, &config.to_models_manager_config())
                    .await,
            ),
            _ => None,
        };
        let child_multi_agent_messages = ResolvedModelMessages::from_model(
            child_model_info.as_ref().unwrap_or(turn.model_info()),
        )
        .multi_agent();
        Some(resolve_usage_hints(
            &config.multi_agent_v2,
            child_multi_agent_messages,
            !config.update_plan_enabled && config.model_catalog.is_none(),
        ))
    } else {
        None
    };
    let (parent_turn_id, root_turn_id, turn_trigger) = match anchor {
        ChildTurnAnchor::ParentTurn => (
            Some(turn.sub_id.clone()),
            turn.turn_metadata_state.root_turn_id(),
            turn.turn_metadata_state.current_turn_trigger(),
        ),
        ChildTurnAnchor::Detached => (None, None, None),
    };
    let effective_model = config.model.clone();
    Ok((
        ChildSpawnSettings {
            config,
            source: spawn_source,
            options: SpawnAgentOptions {
                service_tier: prepared.service_tier,
                request_country: step_context.settings.request_country().clone(),
                fork_parent_spawn_call_id: fork_mode.as_ref().map(|_| call_id.to_string()),
                fork_mode,
                parent_thread_id: Some(session.thread_id),
                parent_turn_id,
                root_turn_id,
                inherited_client_metadata: turn.turn_metadata_state.client_metadata_for_child(),
                turn_trigger,
                environments: Some(step_context.inherited_environments.clone()),
                disabled_plugin_ids: Some(turn.disabled_plugin_ids.clone()),
                multi_agent_v2_usage_hints,
                cyber_access_program: explicit_program_for_spawn(turn),
                cyber_access_program_source: SpawnProgramSource::ParentTurn,
            },
        },
        task_name,
        effective_model,
        effective_role,
    ))
}

#[cfg(test)]
#[path = "thread_spawn_tests.rs"]
mod tests;
