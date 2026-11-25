//! Tests for the workflow host surface.

use super::*;
use serde_json::json;

#[test]
fn host_request_kinds_are_stable() {
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::ReserveAgentCalls {
            count: 1,
            reply: tx
        }
        .kind(),
        "reserve_agent_calls"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::ReleaseAgentCalls {
            count: 1,
            reply: tx
        }
        .kind(),
        "release_agent_calls"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::SpawnAgent {
            opts: AgentOpts::default(),
            reply: tx
        }
        .kind(),
        "spawn_agent"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::ResolveWorkflow {
            name: "x".into(),
            reply: tx
        }
        .kind(),
        "resolve_workflow"
    );
    assert_eq!(
        WorkflowHostRequest::Phase {
            title: "p".into(),
            replayed: false
        }
        .kind(),
        "phase"
    );
    assert_eq!(
        WorkflowHostRequest::Log {
            message: "m".into(),
            replayed: false
        }
        .kind(),
        "log"
    );
    assert_eq!(
        WorkflowHostRequest::Telemetry {
            name: "t".into(),
            fields: json!({}),
            replayed: false
        }
        .kind(),
        "telemetry"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::BudgetQuery { reply: tx }.kind(),
        "budget"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::RenderTemplate {
            name: "t".into(),
            vars: json!({}),
            reply: tx
        }
        .kind(),
        "render_template"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::WriteScratchFile {
            name: "n".into(),
            content: "c".into(),
            reply: tx
        }
        .kind(),
        "write_scratch_file"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::ReadScratchFile {
            name: "n".into(),
            reply: tx
        }
        .kind(),
        "read_scratch_file"
    );
    let (tx, _rx) = oneshot::channel();
    assert_eq!(
        WorkflowHostRequest::GitDiffSince {
            commit: "abc".into(),
            reply: tx
        }
        .kind(),
        "git_diff_since"
    );
}

#[test]
fn agent_opts_defaults_and_json_roundtrip() {
    let opts = AgentOpts {
        prompt: "do work".into(),
        label: Some("worker".into()),
        effort: None,
        output_schema: Some(json!({"type": "object"})),
        ..AgentOpts::default()
    };
    let value = serde_json::to_value(&opts).unwrap();
    assert_eq!(value.get("prompt"), Some(&json!("do work")));
    assert_eq!(value.get("isolation_worktree"), Some(&json!(false)));
    assert!(value.get("effort").is_none());
    let back: AgentOpts = serde_json::from_value(value).unwrap();
    assert_eq!(back.prompt, "do work");
    let schema = back.output_schema.unwrap();
    assert_eq!(schema.get("type"), Some(&json!("object")));
}

#[test]
fn agent_result_and_budget_state_serialize() {
    let result = AgentResult {
        agent_id: "t1".into(),
        success: true,
        output: json!({"ok": true}),
        cancelled: false,
        tokens_used: 12,
        duration_ms: 34,
    };
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value.get("agent_id"), Some(&json!("t1")));
    assert_eq!(value.get("tokens_used"), Some(&json!(12)));

    let budget = BudgetState {
        total: Some(8),
        spent: 3,
        reserved: 1,
        remaining: Some(4),
    };
    let value = serde_json::to_value(budget).unwrap();
    assert_eq!(value.get("total"), Some(&json!(8)));
    assert_eq!(value.get("remaining"), Some(&json!(4)));
}

#[test]
fn host_error_messages_keep_stable_prefixes() {
    assert!(
        HostError::AgentCallQuotaExceeded {
            requested: 3,
            maximum: 2
        }
        .to_string()
        .starts_with("workflow agent-call quota exceeded")
    );
    assert_eq!(
        HostError::BudgetExceeded.to_string(),
        "workflow token budget exceeded"
    );
    assert_eq!(HostError::Cancelled.to_string(), "workflow cancelled");
    assert_eq!(
        HostError::Unsupported("max_output_tokens".into()).to_string(),
        "unsupported in this context: max_output_tokens"
    );
    assert_eq!(
        HostError::Failed("boom".into()).to_string(),
        "host failure: boom"
    );
}
