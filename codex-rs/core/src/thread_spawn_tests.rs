//! TASK-024: workflow child paths, detached turn anchoring, and template capture.
//!
//! These tests use a real `ThreadManager` parent so the captured settings come
//! from the same path production code uses, not a hand-built stub.

#![cfg(test)]

use crate::ThreadManager;
use crate::codex_thread::CodexThread;
use crate::config::ConfigBuilder;
use crate::thread_manager::StartThreadOptions;
use crate::thread_spawn::ChildSpawnSettings;
use crate::thread_spawn::ChildSpawnTemplate;
use crate::thread_spawn::ThreadCreateRequest;
use crate::thread_spawn::ThreadSpawnError;
use crate::thread_spawn::capture_template_from_step;
use crate::thread_spawn::capture_template_from_thread;
use crate::thread_spawn::prepare;
use crate::thread_spawn::prepare_detached;
use crate::thread_spawn::prepare_under_path;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn request(task_name: &str) -> ThreadCreateRequest {
    ThreadCreateRequest {
        task_name: task_name.to_string(),
        fork_mode: None,
        agent_type: None,
        model: None,
        reasoning_effort: None,
    }
}

async fn parent_thread() -> anyhow::Result<(tempfile::TempDir, Arc<CodexThread>)> {
    let home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    config.features.enable(Feature::MultiAgentV2)?;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        home.path().to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let parent = manager
        .start_thread(StartThreadOptions::new(config))
        .await?
        .thread;
    Ok((home, parent))
}

fn spawned_path(source: &SessionSource) -> Option<AgentPath> {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) => {
            agent_path.clone()
        }
        _ => None,
    }
}

fn assert_detached(settings: &ChildSpawnSettings) {
    assert_eq!(settings.options.parent_turn_id, None);
    assert_eq!(settings.options.root_turn_id, None);
    assert_eq!(settings.options.turn_trigger, None);
}

#[tokio::test]
async fn prepare_under_path_accepts_two_level_workflow_run_path() -> anyhow::Result<()> {
    let (_home, parent) = parent_thread().await?;
    let session = &parent.session;
    let step = session
        .capture_step_context(session.new_default_turn().await, &CancellationToken::new())
        .await?;
    let path = AgentPath::from_string("/root/workflow_1234abcd/task_a".to_string())
        .map_err(anyhow::Error::msg)?;

    let (settings, resolved) =
        prepare_under_path(session, &step, "call-1", request("task_a"), path.clone()).await?;

    assert_eq!(resolved, path);
    assert_eq!(spawned_path(&settings.source), Some(path));
    assert_eq!(
        settings.options.parent_turn_id,
        Some(step.turn.sub_id.clone())
    );
    assert_eq!(settings.options.parent_thread_id, Some(session.thread_id));
    parent.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn prepare_keeps_single_segment_behavior() -> anyhow::Result<()> {
    let (_home, parent) = parent_thread().await?;
    let session = &parent.session;
    let step = session
        .capture_step_context(session.new_default_turn().await, &CancellationToken::new())
        .await?;
    let expected = step
        .turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root)
        .join("task_a")
        .map_err(anyhow::Error::msg)?;

    let (settings, resolved) = prepare(session, &step, "call-1", request("task_a")).await?;

    assert_eq!(resolved, expected);
    assert_eq!(
        settings.options.parent_turn_id,
        Some(step.turn.sub_id.clone())
    );
    assert_eq!(spawned_path(&settings.source), Some(expected));
    parent.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn prepare_detached_clears_turn_anchors_but_keeps_source_and_path() -> anyhow::Result<()> {
    let (_home, parent) = parent_thread().await?;
    let session = &parent.session;
    let step = session
        .capture_step_context(session.new_default_turn().await, &CancellationToken::new())
        .await?;
    let template: ChildSpawnTemplate =
        capture_template_from_step(session, &step, "call-1", request("ignored")).await?;
    let expected = template
        .agent_path()
        .join("task_b")
        .map_err(anyhow::Error::msg)?;

    let (settings, resolved) = prepare_detached(&template, "task_b")?;

    assert_eq!(resolved, expected);
    assert_detached(&settings);
    assert_eq!(spawned_path(&settings.source), Some(resolved));
    assert_eq!(settings.options.parent_thread_id, Some(session.thread_id));
    assert_eq!(settings.config.model, template.config().model);
    parent.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn capture_template_from_thread_matches_step_capture() -> anyhow::Result<()> {
    let (_home, parent) = parent_thread().await?;
    let session = &parent.session;
    let step = session
        .capture_step_context(session.new_default_turn().await, &CancellationToken::new())
        .await?;
    let from_step =
        capture_template_from_step(session, &step, "call-1", request("ignored")).await?;
    let from_thread = capture_template_from_thread(&parent, "call-1", request("ignored")).await?;

    assert_eq!(from_thread.agent_path(), from_step.agent_path());
    assert_eq!(from_thread.model(), from_step.model());
    assert_eq!(from_thread.role_name(), from_step.role_name());
    assert_eq!(from_thread.config().model, from_step.config().model);
    parent.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test]
async fn prepare_detached_rejects_multi_segment_task_name() -> anyhow::Result<()> {
    let (_home, parent) = parent_thread().await?;
    let session = &parent.session;
    let step = session
        .capture_step_context(session.new_default_turn().await, &CancellationToken::new())
        .await?;
    let template = capture_template_from_step(session, &step, "call-1", request("ignored")).await?;

    let result = prepare_detached(&template, "nested/task");

    assert!(matches!(result, Err(ThreadSpawnError::Preparation(_))));
    parent.shutdown_and_wait().await?;
    Ok(())
}
