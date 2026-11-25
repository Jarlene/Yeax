//! Tests for the TUI workflow view.

use super::WorkflowCommand;
use super::WorkflowCommandError;
use super::WorkflowRequest;
use super::WorkflowStartArgs;
use super::build_workflow_request;
use super::parse_workflow_command;
use super::render_request_error;
use super::start_prompt;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use serde_json::json;

fn start(args: WorkflowStartArgs) -> WorkflowCommand {
    WorkflowCommand::Start(args)
}

#[test]
fn parses_runs_and_list_as_completed_inclusive_lists() {
    for input in ["runs", "list"] {
        assert_eq!(
            parse_workflow_command(input),
            Ok(WorkflowCommand::Runs {
                include_completed: true
            })
        );
    }
    assert_eq!(parse_workflow_command(""), Err(WorkflowCommandError::Usage));
    assert_eq!(
        parse_workflow_command("runs extra"),
        Err(WorkflowCommandError::Usage)
    );
}

#[test]
fn parses_control_subcommands_with_optional_run_id() {
    assert_eq!(
        parse_workflow_command("pause"),
        Ok(WorkflowCommand::Pause { run_id: None })
    );
    assert_eq!(
        parse_workflow_command("resume run-7"),
        Ok(WorkflowCommand::Resume {
            run_id: Some("run-7".to_string())
        })
    );
    assert_eq!(
        parse_workflow_command("stop run-8"),
        Ok(WorkflowCommand::Stop {
            run_id: Some("run-8".to_string())
        })
    );
    assert_eq!(
        parse_workflow_command("pause a b"),
        Err(WorkflowCommandError::Usage)
    );
}

#[test]
fn parses_save_with_a_required_name() {
    assert_eq!(
        parse_workflow_command("save team-probe"),
        Ok(WorkflowCommand::Save {
            name: "team-probe".to_string(),
            run_id: None
        })
    );
    assert_eq!(
        parse_workflow_command("save team-probe run-1"),
        Ok(WorkflowCommand::Save {
            name: "team-probe".to_string(),
            run_id: Some("run-1".to_string())
        })
    );
    assert_eq!(
        parse_workflow_command("save"),
        Err(WorkflowCommandError::Usage)
    );
}

#[test]
fn parses_start_flags_plain_text_and_json_arguments() {
    assert_eq!(
        parse_workflow_command("hello-workflow research rust"),
        Ok(start(WorkflowStartArgs {
            name: "hello-workflow".to_string(),
            args_text: Some("research rust".to_string()),
            ..Default::default()
        }))
    );
    assert_eq!(
        parse_workflow_command(
            "hello-workflow --agent-budget 8 --effort high {\"query\": \"rust\"}"
        ),
        Ok(start(WorkflowStartArgs {
            name: "hello-workflow".to_string(),
            agent_budget: Some(8),
            effort: Some("high".to_string()),
            args: Some(json!({"query": "rust"})),
            args_text: None,
        }))
    );
    assert_eq!(
        parse_workflow_command("hello-workflow --agent-budget 0"),
        Err(WorkflowCommandError::InvalidAgentBudget("0".to_string()))
    );
    assert_eq!(
        parse_workflow_command("hello-workflow --agent-budget 1025"),
        Err(WorkflowCommandError::InvalidAgentBudget("1025".to_string()))
    );
    assert_eq!(
        parse_workflow_command("hello-workflow --agent-budget 8 --agent-budget 9"),
        Err(WorkflowCommandError::DuplicateFlag("--agent-budget"))
    );
    assert_eq!(
        parse_workflow_command("hello-workflow --bogus"),
        Err(WorkflowCommandError::UnknownFlag("--bogus".to_string()))
    );
    assert!(matches!(
        parse_workflow_command("hello-workflow {not json}"),
        Err(WorkflowCommandError::InvalidArgsJson(_))
    ));
    assert!(matches!(
        parse_workflow_command("hello-workflow [1, 2]"),
        Err(WorkflowCommandError::InvalidArgsJson(_))
    ));
}

#[test]
fn start_prompt_uses_the_workflow_tool_and_maps_plain_text_arguments() {
    let prompt = start_prompt(&WorkflowStartArgs {
        name: "hello-workflow".to_string(),
        agent_budget: Some(4),
        effort: Some("high".to_string()),
        args: None,
        args_text: Some("research rust".to_string()),
    });
    assert!(prompt.starts_with("Call the `workflow` tool with "));
    assert!(prompt.contains("\"name\":\"hello-workflow\""));
    assert!(prompt.contains("\"agent_budget\":4"));
    assert!(prompt.contains("\"effort\":\"high\""));
    assert!(prompt.contains("\"query\":\"research rust\""));
    assert!(prompt.contains("\"objective\":\"research rust\""));

    let prompt = start_prompt(&WorkflowStartArgs {
        name: "hello-workflow".to_string(),
        args: Some(json!({"query": "json"})),
        ..Default::default()
    });
    assert!(prompt.contains("\"args\":{\"query\":\"json\"}"));
    assert!(!prompt.contains("agent_budget"));
}

#[test]
fn start_is_not_a_control_request() {
    assert_eq!(
        build_workflow_request(
            &start(WorkflowStartArgs {
                name: "hello-workflow".to_string(),
                ..Default::default()
            }),
            "thread-1",
            None
        ),
        Err(WorkflowCommandError::NotAControlRequest)
    );
}

#[test]
fn control_requests_always_carry_the_thread_id() {
    let list = build_workflow_request(
        &WorkflowCommand::Runs {
            include_completed: true,
        },
        "thread-1",
        None,
    )
    .unwrap();
    assert_eq!(
        list,
        WorkflowRequest::List(codex_app_server_protocol::WorkflowListParams {
            thread_id: "thread-1".to_string(),
            include_completed: true,
        })
    );

    let pause = build_workflow_request(
        &WorkflowCommand::Pause {
            run_id: Some("run-1".to_string()),
        },
        "thread-1",
        None,
    )
    .unwrap();
    assert_eq!(
        pause,
        WorkflowRequest::Pause(codex_app_server_protocol::WorkflowPauseParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
        })
    );

    let save = build_workflow_request(
        &WorkflowCommand::Save {
            name: "team-probe".to_string(),
            run_id: Some("run-2".to_string()),
        },
        "thread-1",
        None,
    )
    .unwrap();
    assert_eq!(
        save,
        WorkflowRequest::Save(codex_app_server_protocol::WorkflowSaveParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-2".to_string(),
            name: "team-probe".to_string(),
        })
    );
    // The request maps to the user-scope `workflow/save` method only.
    assert_eq!(save.method(), "workflow/save");
    assert_eq!(
        save.into_client_request(RequestId::String("req".to_string())),
        ClientRequest::WorkflowSave {
            request_id: RequestId::String("req".to_string()),
            params: codex_app_server_protocol::WorkflowSaveParams {
                thread_id: "thread-1".to_string(),
                run_id: "run-2".to_string(),
                name: "team-probe".to_string(),
            },
        }
    );
}

#[test]
fn omitted_run_id_uses_the_single_active_run_or_fails() {
    assert_eq!(
        build_workflow_request(&WorkflowCommand::Stop { run_id: None }, "thread-1", None),
        Err(WorkflowCommandError::MissingRunId)
    );
    let request = build_workflow_request(
        &WorkflowCommand::Stop { run_id: None },
        "thread-1",
        Some("run-9"),
    )
    .unwrap();
    assert_eq!(
        request,
        WorkflowRequest::Stop(codex_app_server_protocol::WorkflowStopParams {
            thread_id: "thread-1".to_string(),
            run_id: "run-9".to_string(),
        })
    );
}

#[test]
fn renders_stable_server_error_codes_and_details() {
    let error = TypedRequestError::Server {
        method: "workflow/pause".to_string(),
        source: JSONRPCErrorError {
            code: -32602,
            message: "no such run".to_string(),
            data: Some(json!({
                "code": "workflow_control_unknown_run",
                "detail": "run-404"
            })),
        },
    };
    assert_eq!(
        render_request_error(&error),
        vec![
            "workflow/pause failed: workflow_control_unknown_run (no such run)".to_string(),
            "detail: run-404".to_string(),
        ]
    );
}

#[test]
fn unregistered_or_missing_error_data_renders_as_unknown() {
    for data in [
        None,
        Some(json!({"code": "workflow_internal_error"})),
        Some(json!("not an object")),
    ] {
        let error = TypedRequestError::Server {
            method: "workflow/list".to_string(),
            source: JSONRPCErrorError {
                code: -32603,
                message: "boom".to_string(),
                data,
            },
        };
        assert_eq!(
            render_request_error(&error),
            vec!["workflow/list failed: unknown (boom)".to_string()]
        );
    }
}

#[test]
fn transport_and_deserialize_errors_stay_local_and_do_not_fake_run_state() {
    let transport = TypedRequestError::Transport {
        method: "workflow/list".to_string(),
        source: std::io::Error::other("pipe closed"),
    };
    assert_eq!(
        render_request_error(&transport),
        vec!["workflow/list transport error: pipe closed".to_string()]
    );
    let deserialize = TypedRequestError::Deserialize {
        method: "workflow/list".to_string(),
        source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
    };
    assert!(
        render_request_error(&deserialize)[0].starts_with("workflow/list response decode error:")
    );
}
