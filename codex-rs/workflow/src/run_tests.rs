//! Tests for workflow run types.

use super::*;
use std::str::FromStr;

#[test]
fn pause_kind_parses_aliases() {
    assert_eq!(PauseKind::from_str("user"), Ok(PauseKind::User));
    assert_eq!(PauseKind::from_str("back_off"), Ok(PauseKind::BackOff));
    assert_eq!(PauseKind::from_str("backoff"), Ok(PauseKind::BackOff));
    assert_eq!(
        PauseKind::from_str("no_progress"),
        Ok(PauseKind::NoProgress)
    );
    assert_eq!(
        PauseKind::from_str("verification"),
        Ok(PauseKind::Verification)
    );
    assert_eq!(PauseKind::from_str("blocked"), Ok(PauseKind::Verification));
    assert_eq!(PauseKind::from_str("infra"), Ok(PauseKind::Infra));
    assert!(PauseKind::from_str("unknown").is_err());
}

#[test]
fn pause_kind_serializes_snake_case() {
    assert_eq!(
        serde_json::to_value(PauseKind::BackOff).unwrap(),
        "back_off"
    );
    assert_eq!(
        serde_json::to_value(PauseKind::NoProgress).unwrap(),
        "no_progress"
    );
}

#[test]
fn workflow_outcome_is_tagged() {
    let value = serde_json::to_value(WorkflowOutcome::Completed {
        result: serde_json::json!({"summary": "ok"}),
    })
    .unwrap();
    assert_eq!(value.get("outcome"), Some(&serde_json::json!("completed")));
    assert_eq!(
        value.get("result").and_then(|result| result.get("summary")),
        Some(&serde_json::json!("ok"))
    );

    let value = serde_json::to_value(WorkflowOutcome::Paused {
        kind: PauseKind::User,
        message: "paused".into(),
    })
    .unwrap();
    assert_eq!(value.get("outcome"), Some(&serde_json::json!("paused")));
    assert_eq!(value.get("kind"), Some(&serde_json::json!("user")));

    let value = serde_json::to_value(WorkflowOutcome::Cancelled).unwrap();
    assert_eq!(value.get("outcome"), Some(&serde_json::json!("cancelled")));
}
