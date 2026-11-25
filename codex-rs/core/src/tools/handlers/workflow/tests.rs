//! Tool-level tests for the `workflow` handler contract.

use codex_tools::ToolSpec;
use serde_json::json;

use crate::tools::registry::ToolExecutor;
use crate::workflow::WorkflowRequest;
use crate::workflow::request_service::WorkflowSource;

use super::description::WORKFLOW_DESCRIPTION;
use super::description::WORKFLOW_TOOL_NAME;
use super::description::create_workflow_tool;
use super::run::WorkflowHandler;

fn parse(value: serde_json::Value) -> WorkflowRequest {
    serde_json::from_value(value).expect("workflow request must parse")
}

#[test]
fn description_mentions_parallel_pipeline_and_disabled_apis() {
    assert!(WORKFLOW_DESCRIPTION.contains("parallel"));
    assert!(WORKFLOW_DESCRIPTION.contains("pipeline"));
    for disabled in ["eval", "timestamp", "sleep", "exit"] {
        assert!(
            WORKFLOW_DESCRIPTION.contains(disabled),
            "description must list `{disabled}` as unavailable"
        );
    }
    assert!(WORKFLOW_DESCRIPTION.contains("option map"));
    assert!(WORKFLOW_DESCRIPTION.contains("serially"));
}

#[test]
fn tool_spec_exposes_tagged_source_and_controls() {
    let spec = create_workflow_tool();
    let ToolSpec::Function(tool) = spec else {
        panic!("workflow must be a plain function tool");
    };
    assert_eq!(tool.name, WORKFLOW_TOOL_NAME);
    let properties = tool.parameters.properties.expect("parameters");
    for field in ["source", "agent_budget", "args", "validate_only"] {
        assert!(properties.contains_key(field), "missing `{field}`");
    }
    let source = properties.get("source").expect("source");
    assert_eq!(
        source.one_of.as_ref().map(|variants| variants.len()),
        Some(6)
    );
}

#[test]
fn handler_name_is_plain_workflow() {
    assert_eq!(WorkflowHandler.tool_name().to_string(), "workflow");
}

#[test]
fn all_six_tagged_sources_parse() {
    let cases = [
        json!({"source": {"type": "name", "name": "hello-workflow"}}),
        json!({"source": {"type": "script", "script": "let x = 1;"}}),
        json!({"source": {"type": "script_path", "script_path": "a.rhai"}}),
        json!({"source": {"type": "resume", "resume_from_run_id": "wf_1"}}),
        json!({"source": {"type": "pause"}}),
        json!({"source": {"type": "stop", "run_id": "wf_1"}}),
    ];
    assert!(matches!(
        parse(cases[0].clone()).source,
        WorkflowSource::Name { .. }
    ));
    assert!(matches!(
        parse(cases[1].clone()).source,
        WorkflowSource::Script { .. }
    ));
    assert!(matches!(
        parse(cases[2].clone()).source,
        WorkflowSource::ScriptPath { .. }
    ));
    assert!(matches!(
        parse(cases[3].clone()).source,
        WorkflowSource::Resume { .. }
    ));
    assert!(matches!(
        parse(cases[4].clone()).source,
        WorkflowSource::Pause { run_id: None }
    ));
    assert!(matches!(
        parse(cases[5].clone()).source,
        WorkflowSource::Stop { run_id: Some(_) }
    ));
}

#[test]
fn agent_budget_range_is_enforced_before_launch() {
    let too_small = parse(json!({
        "source": {"type": "name", "name": "wall"},
        "agent_budget": 0
    }));
    assert!(too_small.validate().is_err());
    let too_large = parse(json!({
        "source": {"type": "name", "name": "wall"},
        "agent_budget": 1025
    }));
    assert!(too_large.validate().is_err());
    let ok = parse(json!({
        "source": {"type": "name", "name": "wall"},
        "agent_budget": 1024
    }));
    assert!(ok.validate().is_ok());
}

#[test]
fn pause_and_stop_reject_launch_only_fields() {
    for field in [
        json!({"agent_budget": 2}),
        json!({"args": {}}),
        json!({"validate_only": true}),
    ] {
        let mut value = json!({"source": {"type": "pause"}});
        if let (Some(target), Some(extra)) = (value.as_object_mut(), field.as_object()) {
            target.extend(extra.clone());
        }
        assert!(parse(value).validate().is_err());
    }
}

#[test]
fn resume_rejects_validate_only() {
    let request = parse(json!({
        "source": {"type": "resume", "resume_from_run_id": "wf_1"},
        "validate_only": true
    }));
    assert!(request.validate().is_err());
}
