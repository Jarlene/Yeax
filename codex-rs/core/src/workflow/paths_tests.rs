//! Tests for workflow path scoping.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

fn home() -> PathBuf {
    PathBuf::from("/tmp/codex-home")
}

#[test]
fn rollout_parent_wins_and_appends_session_id() {
    let paths = WorkflowPaths::resolve_with_git_root(
        home(),
        None,
        "sess-01",
        Some(Path::new("/work/rollout/session.jsonl")),
    )
    .expect("resolve");
    assert_eq!(
        paths.session_root(),
        Path::new("/work/rollout/workflows/sess-01")
    );
    assert_eq!(paths.session_id(), "sess-01");
}

#[test]
fn missing_rollout_falls_back_to_codex_home_sessions() {
    let paths =
        WorkflowPaths::resolve_with_git_root(home(), None, "sess_2", None).expect("resolve");
    assert_eq!(
        paths.session_root(),
        Path::new("/tmp/codex-home/workflows/sessions/sess_2")
    );
}

#[test]
fn user_bundled_and_project_directories_are_derived_once() {
    let paths = WorkflowPaths::resolve_with_git_root(home(), Some(Path::new("/repo")), "s", None)
        .expect("resolve");
    assert_eq!(paths.user_dir(), Path::new("/tmp/codex-home/workflows"));
    assert_eq!(
        paths.bundled_dir(),
        Path::new("/tmp/codex-home/bundled/workflows")
    );
    assert_eq!(
        paths.project_dir(),
        Some(Path::new("/repo/.codex/workflows"))
    );
    assert_eq!(
        paths.manifest_path(),
        Path::new("/tmp/codex-home/workflows/sessions/s/manifest.json")
    );
    assert_eq!(
        paths.bundled_index_path(),
        Path::new("/tmp/codex-home/bundled/workflows/.bundled-index.json")
    );
    assert_eq!(
        paths.bundled_index_lock_path(),
        Path::new("/tmp/codex-home/bundled/workflows/.bundled-index.lock")
    );
}

#[test]
fn no_git_root_disables_project_scope() {
    let paths = WorkflowPaths::resolve_with_git_root(home(), None, "s", None).expect("resolve");
    assert_eq!(paths.project_dir(), None);
}

#[test]
fn invalid_session_ids_are_rejected() {
    for value in [
        "",
        "..",
        ".",
        "a/b",
        "a\\b",
        "sp ace",
        "sess.1",
        "x".repeat(129).as_str(),
    ] {
        let error = WorkflowPaths::resolve_with_git_root(home(), None, value, None)
            .expect_err("must reject");
        assert!(matches!(error, WorkflowPathError::InvalidSessionId(_)));
    }
}

#[test]
fn run_directory_is_validated() {
    let paths = WorkflowPaths::resolve_with_git_root(home(), None, "s", None).expect("resolve");
    assert_eq!(
        paths.run_dir("wf_abc-123").expect("run dir"),
        Path::new("/tmp/codex-home/workflows/sessions/s/wf_abc-123")
    );
    for value in ["", ".", "..", "a/b", "a\\b", "run..1"] {
        let error = paths.run_dir(value).expect_err("must reject");
        assert!(matches!(error, WorkflowPathError::InvalidRunId(_)));
    }
}

#[test]
fn session_root_is_stable_across_rollout_changes() {
    let paths = WorkflowPaths::resolve_with_git_root(home(), None, "s", None).expect("resolve");
    let cached = Arc::new(paths);
    let before = cached.session_root().to_path_buf();
    // A rollout appearing later is a different process's input; the cached
    // instance must not move.
    let other = WorkflowPaths::resolve_with_git_root(
        home(),
        None,
        "s",
        Some(Path::new("/work/rollout/session.jsonl")),
    )
    .expect("resolve");
    assert_eq!(cached.session_root(), before.as_path());
    assert_ne!(other.session_root(), before.as_path());
}

#[test]
fn no_grok_path_component_is_produced() {
    let paths = WorkflowPaths::resolve_with_git_root(
        home(),
        Some(Path::new("/repo")),
        "s",
        Some(Path::new("/work/rollout/session.jsonl")),
    )
    .expect("resolve");
    let display = format!(
        "{} {} {} {} {}",
        paths.user_dir().display(),
        paths.bundled_dir().display(),
        paths.project_dir().unwrap().display(),
        paths.session_root().display(),
        paths.run_dir("r").unwrap().display()
    );
    assert!(!display.contains(".grok"));
}

#[test]
fn ensure_session_root_creates_directories() {
    let dir = tempfile::TempDir::new().expect("temp");
    let paths = WorkflowPaths::resolve_with_git_root(
        dir.path().to_path_buf(),
        None,
        "s1",
        Some(&dir.path().join("rollout").join("session.jsonl")),
    )
    .expect("resolve");
    paths.ensure_session_root().expect("create");
    assert!(paths.session_root().is_dir());
}
