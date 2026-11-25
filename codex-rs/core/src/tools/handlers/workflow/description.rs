//! Tool description and JSON schema for the `workflow` tool.
//!
//! Kept in one place so the model-visible contract and the acceptance criteria
//! cannot drift: `parallel` takes an array of option maps, `pipeline` runs its
//! items serially, and `eval`/`timestamp`/`sleep`/`exit` are not available.

use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;

pub(crate) const WORKFLOW_TOOL_NAME: &str = "workflow";

pub(crate) const WORKFLOW_DESCRIPTION: &str = "\
Run a registered Rhai workflow in the background and return immediately with \
its run id; this tool never polls for completion.\n\
\n\
`source` is a tagged union with exactly one of:\n\
- {type: \"name\", name}: run a workflow resolved from the four scopes.\n\
- {type: \"script\", script}: run an inline Rhai script.\n\
- {type: \"script_path\", script_path}: run a script file relative to the session cwd.\n\
- {type: \"resume\", resume_from_run_id}: rerun the frozen script of a previous run.\n\
- {type: \"pause\", run_id?}: pause the only active run, or the named run.\n\
- {type: \"stop\", run_id?}: stop the only active run, or the named run.\n\
\n\
Script surface: `parallel([{prompt: ...}, ...])` accepts an array of option \
maps; `pipeline([...])` executes its items serially. `eval`, `timestamp`, \
`sleep`, and `exit` are not available inside workflow scripts.\n\
\n\
`agent_budget` is 1..=1024; `pause`/`stop` reject `agent_budget`, `args`, and \
`validate_only`; `resume` rejects `validate_only`. Use `validate_only: true` to \
dry-run a script without creating a run.";

fn tagged_source(kind: &str, extra: BTreeMap<String, JsonSchema>) -> JsonSchema {
    let mut properties = BTreeMap::from([(
        "type".to_string(),
        JsonSchema::string_enum(vec![json!(kind)], Some(format!("Use the `{kind}` source."))),
    )]);
    properties.extend(extra);
    JsonSchema::object(
        properties,
        Some(vec!["type".to_string()]),
        Some(false.into()),
    )
}

fn source_schema() -> JsonSchema {
    let name = tagged_source(
        "name",
        BTreeMap::from([(
            "name".to_string(),
            JsonSchema::string(Some("Registered workflow name.".to_string())),
        )]),
    );
    let script = tagged_source(
        "script",
        BTreeMap::from([(
            "script".to_string(),
            JsonSchema::string(Some("Inline Rhai workflow script.".to_string())),
        )]),
    );
    let script_path = tagged_source(
        "script_path",
        BTreeMap::from([(
            "script_path".to_string(),
            JsonSchema::string(Some("Path to a Rhai workflow script.".to_string())),
        )]),
    );
    let resume = tagged_source(
        "resume",
        BTreeMap::from([(
            "resume_from_run_id".to_string(),
            JsonSchema::string(Some(
                "Run id (or unambiguous prefix) whose frozen script is rerun.".to_string(),
            )),
        )]),
    );
    let pause = tagged_source(
        "pause",
        BTreeMap::from([(
            "run_id".to_string(),
            JsonSchema::string(Some(
                "Optional run id; required when more than one run is active.".to_string(),
            )),
        )]),
    );
    let stop = tagged_source(
        "stop",
        BTreeMap::from([(
            "run_id".to_string(),
            JsonSchema::string(Some(
                "Optional run id; required when more than one run is active.".to_string(),
            )),
        )]),
    );
    JsonSchema::one_of(
        vec![name, script, script_path, resume, pause, stop],
        Some("Exactly one tagged workflow source.".to_string()),
    )
}

pub(crate) fn create_workflow_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        ("source".to_string(), source_schema()),
        (
            "agent_budget".to_string(),
            JsonSchema::integer(Some(
                "Maximum agent calls for this run, 1..=1024.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema::object(BTreeMap::new(), None, Some(true.into())),
        ),
        (
            "validate_only".to_string(),
            JsonSchema::boolean(Some(
                "Dry-run the script without creating a run.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: WORKFLOW_TOOL_NAME.to_string(),
        description: WORKFLOW_DESCRIPTION.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["source".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
