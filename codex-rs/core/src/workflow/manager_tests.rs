//! Tests for the workflow manager.

use super::*;

/// Design 03 section 6.2 is a table, so the test is a table: every branch of
/// the normalization pipeline must have an explicit row.
#[test]
fn task_name_normalization_table() {
    let cases: &[(&str, &str)] = &[
        // plain labels keep their readable shape
        ("Review changes", "review_changes"),
        ("Fix-Bug#42", "fix_bug_42"),
        ("UPPER", "upper"),
        // surrounding whitespace is not part of the name
        ("  spaced  ", "spaced"),
        // runs of separators collapse, leading/trailing ones disappear
        ("--a___b--", "a_b"),
        ("a b", "a_b"),
        ("已经包含中文", "agent"),
        // empty inputs fall back to `agent`
        ("", "agent"),
        ("   ", "agent"),
        ("!!!", "agent"),
        // path-shaped inputs are prefixed and stay distinguishable
        ("root", "wf_root"),
        (".", "wf_dot"),
        ("..", "wf_dotdot"),
        // non-ASCII collapses to `_` and then to the fallback
        ("中文 task", "task"),
    ];
    for (source, expected) in cases {
        assert_eq!(
            normalize_task_name(source, &[]),
            *expected,
            "source {source:?}"
        );
    }
}

#[test]
fn task_name_is_ascii_and_bounded() {
    let source = "A".repeat(200);
    let name = normalize_task_name(&source, &[]);
    assert_eq!(name.len(), MAX_TASK_NAME_BYTES);
    assert!(
        name.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    );
}

#[test]
fn task_name_truncation_keeps_a_usable_tail() {
    let source = format!("{}{}", "x".repeat(46), "_tail");
    let name = normalize_task_name(&source, &[]);
    assert_eq!(name.len(), MAX_TASK_NAME_BYTES);
    assert!(!name.ends_with('_'));
}

/// Same run: duplicates get `_2`, `_3`; an explicit duplicate source keeps
/// incrementing instead of restarting.
#[test]
fn task_names_are_unique_inside_one_run() {
    let mut names = TaskNames::default();
    assert_eq!(names.next("Plan the change"), "plan_the_change");
    assert_eq!(names.next("Plan the change"), "plan_the_change_2");
    assert_eq!(names.next("Plan the change"), "plan_the_change_3");
    assert_eq!(names.next("root"), "wf_root");
    assert_eq!(names.next("root"), "wf_root_2");
    assert_eq!(names.used().len(), 5);
}

#[test]
fn task_names_restart_for_a_new_run() {
    let mut first = TaskNames::default();
    let mut second = TaskNames::default();
    assert_eq!(first.next("Plan"), "plan");
    assert_eq!(second.next("Plan"), "plan");
}

#[test]
fn duplicate_suffix_keeps_the_byte_bound() {
    let mut names = TaskNames::default();
    let long = "y".repeat(MAX_TASK_NAME_BYTES);
    let first = names.next(&long);
    let second = names.next(&long);
    assert!(first.len() <= MAX_TASK_NAME_BYTES);
    assert!(second.len() <= MAX_TASK_NAME_BYTES);
    assert!(second.ends_with("_2"));
    assert_ne!(first, second);
}

#[test]
fn control_error_messages_are_actionable() {
    assert!(
        WorkflowControlError::Disabled
            .to_string()
            .contains("features")
    );
    assert_eq!(
        WorkflowControlError::MaxActiveRunsExceeded {
            active: 4,
            max: MAX_ACTIVE_RUNS
        }
        .to_string(),
        "too many active workflow runs (4 > 4)"
    );
    assert!(
        WorkflowControlError::SaveFailed {
            path: std::path::PathBuf::from("/tmp/x.rhai"),
            detail: "Permission denied (os error 13)".into(),
        }
        .to_string()
        .contains("os error 13")
    );
}

/// TASK-004: `[workflow]` must reach the runtime instead of staying as an
/// unread accessor pair.
#[test]
fn limits_are_resolved_from_workflow_config() {
    use codex_config::config_toml::WorkflowConfigToml;

    let config = WorkflowConfigToml {
        max_active_runs: Some(2),
        default_agent_budget: Some(7),
        max_concurrent_agents: Some(3),
    };
    config.validate().expect("valid config");

    assert_eq!(
        WorkflowLimits::from_config(Some(&config)),
        WorkflowLimits {
            max_active_runs: 2,
            default_agent_budget: 7,
            max_concurrent_agents: 3,
        }
    );
    assert_eq!(WorkflowLimits::from_config(None), WorkflowLimits::default());
}

#[test]
fn invalid_workflow_config_is_rejected() {
    use codex_config::config_toml::WorkflowConfigToml;

    let config = WorkflowConfigToml {
        max_active_runs: Some(5),
        ..WorkflowConfigToml::default()
    };
    assert!(config.validate().is_err());
}
