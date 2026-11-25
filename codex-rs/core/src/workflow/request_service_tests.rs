//! Tests for the workflow request service.

use std::sync::Arc;

use serde_json::json;

use super::super::registry::MAX_WORKFLOW_SOURCE_BYTES;
use super::*;

fn script(name: &str) -> String {
    format!("let meta = #{{ name: \"{name}\", description: \"d\" }};\ncomplete(\"ok\");")
}

fn paths(home: &Path) -> Arc<WorkflowPaths> {
    Arc::new(
        WorkflowPaths::resolve_with_git_root(home.to_path_buf(), None, "sess-1", None)
            .expect("paths"),
    )
}

#[derive(Default)]
struct MockRuns {
    runs: Vec<(String, String, Value)>,
    active: Vec<String>,
}

impl RunScriptLookup for MockRuns {
    fn find_run_id(&self, key: &str) -> Option<String> {
        if let Some((id, _, _)) = self.runs.iter().find(|(id, _, _)| id == key) {
            return Some(id.clone());
        }
        let matches: Vec<_> = self
            .runs
            .iter()
            .filter(|(id, _, _)| id.starts_with(key))
            .map(|(id, _, _)| id.clone())
            .collect();
        (matches.len() == 1).then(|| matches[0].clone())
    }

    fn script_copy_for(&self, run_id: &str) -> Option<String> {
        self.runs
            .iter()
            .find(|(id, _, _)| id == run_id)
            .map(|(_, script, _)| script.clone())
    }

    fn args_copy_for(&self, run_id: &str) -> Option<Value> {
        self.runs
            .iter()
            .find(|(id, _, _)| id == run_id)
            .map(|(_, _, args)| args.clone())
    }

    fn active_run_ids(&self) -> Vec<String> {
        self.active.clone()
    }
}

fn request(value: Value) -> WorkflowRequest {
    serde_json::from_value(value).expect("request")
}

#[test]
fn six_tagged_sources_parse() {
    let cases = [
        (json!({"type":"name","name":"hello-workflow"}), true, false),
        (json!({"type":"script","script":"x"}), false, false),
        (
            json!({"type":"script_path","script_path":"a/b.rhai"}),
            false,
            false,
        ),
        (
            json!({"type":"resume","resume_from_run_id":"wf_1"}),
            false,
            true,
        ),
        (json!({"type":"pause","run_id":"wf_1"}), false, false),
        (json!({"type":"stop"}), false, false),
    ];
    for (value, _is_validate, is_resume) in cases {
        let parsed = request(json!({"source": value}));
        assert_eq!(parsed.source.is_resume(), is_resume);
        assert_eq!(
            parsed.source.is_control(),
            matches!(
                parsed.source,
                WorkflowSource::Pause { .. } | WorkflowSource::Stop { .. }
            )
        );
    }
}

#[test]
fn parameter_rules_are_enforced() {
    for budget in [0, 1025] {
        let error = request(json!({
            "source": {"type":"name","name":"hello-workflow"},
            "agent_budget": budget,
        }))
        .validate()
        .expect_err("budget");
        assert_eq!(error.code(), "workflow_invalid_input");
    }
    request(json!({
        "source": {"type":"name","name":"hello-workflow"},
        "agent_budget": 1024,
    }))
    .validate()
    .expect("boundary budget");

    for value in [
        json!({"source":{"type":"pause"},"args":{"a":1}}),
        json!({"source":{"type":"stop"},"agent_budget":3}),
        json!({"source":{"type":"pause"},"validate_only":true}),
        json!({"source":{"type":"resume","resume_from_run_id":"wf_1"},"validate_only":true}),
    ] {
        let error = request(value).validate().expect_err("mutually exclusive");
        assert_eq!(error.code(), "workflow_invalid_input");
    }
}

#[test]
fn disabled_feature_rejects_before_any_resolution() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path());
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    let runs = MockRuns::default();
    let service = RequestService::new(&registry, &runs, paths, home.path().to_path_buf(), false);
    let error = service
        .prepare(&request(
            json!({"source":{"type":"name","name":"hello-workflow"}}),
        ))
        .expect_err("disabled");
    assert_eq!(error.code(), "workflows_disabled");
}

#[test]
fn name_script_and_path_route_to_launch_or_validate() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path());
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    let runs = MockRuns::default();
    let service = RequestService::new(
        &registry,
        &runs,
        paths.clone(),
        home.path().to_path_buf(),
        true,
    );

    let prepared = service
        .prepare(&request(
            json!({"source":{"type":"name","name":"hello-workflow"}}),
        ))
        .expect("name");
    assert!(matches!(
        prepared,
        PreparedRequest::Launch {
            resume_run_id: None,
            ..
        }
    ));

    let prepared = service
        .prepare(&request(json!({
            "source":{"type":"script","script":script("inline-one")},
            "validate_only": true,
        })))
        .expect("inline validate");
    let PreparedRequest::Validate {
        workflow,
        agent_budget,
        ..
    } = prepared
    else {
        panic!("expected validate");
    };
    assert_eq!(workflow.meta.name, "inline-one");
    assert_eq!(agent_budget, DEFAULT_AGENT_BUDGET);

    std::fs::create_dir_all(paths.user_dir()).expect("user dir");
    let file = paths.user_dir().join("from-path.rhai");
    std::fs::write(&file, script("from-path")).expect("write");
    let prepared = service
        .prepare(&request(json!({
            "source":{"type":"script_path","script_path":file.to_string_lossy()},
        })))
        .expect("path");
    assert!(matches!(prepared, PreparedRequest::Launch { .. }));
}

#[test]
fn resume_requires_a_known_run_with_a_persisted_script() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path());
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    let runs = MockRuns {
        runs: vec![(
            "wf_0192f0c4a1b2".to_string(),
            script("resumed"),
            json!({"objective":"again"}),
        )],
        active: Vec::new(),
    };
    let service = RequestService::new(&registry, &runs, paths, home.path().to_path_buf(), true);
    let prepared = service
        .prepare(&request(json!({
            "source":{"type":"resume","resume_from_run_id":"wf_0192"},
        })))
        .expect("resume prefix");
    let PreparedRequest::Launch {
        workflow,
        args,
        resume_run_id,
        ..
    } = prepared
    else {
        panic!("expected launch");
    };
    assert_eq!(workflow.meta.name, "resumed");
    assert_eq!(args, json!({"objective":"again"}));
    assert_eq!(resume_run_id.as_deref(), Some("wf_0192f0c4a1b2"));

    let error = service
        .prepare(&request(json!({
            "source":{"type":"resume","resume_from_run_id":"wf_missing"},
        })))
        .expect_err("unknown");
    assert_eq!(error.code(), "workflow_resume_unknown_run");
}

#[test]
fn control_requires_explicit_or_unique_active_run() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path());
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    let mut runs = MockRuns::default();
    let cwd = home.path().to_path_buf();
    let service = RequestService::new(&registry, &runs, paths.clone(), cwd.clone(), true);

    let error = service
        .prepare(&request(json!({"source":{"type":"stop"}})))
        .expect_err("none active");
    assert_eq!(error.code(), "workflow_control_unknown_run");

    runs.active = vec!["wf_a".to_string(), "wf_b".to_string()];
    let service = RequestService::new(&registry, &runs, paths.clone(), cwd.clone(), true);
    let error = service
        .prepare(&request(json!({"source":{"type":"pause"}})))
        .expect_err("ambiguous");
    assert_eq!(error.code(), "workflow_invalid_input");
    assert!(error.detail().contains("wf_a"));
    assert!(error.detail().contains("wf_b"));

    runs.active = vec!["wf_a".to_string()];
    runs.runs
        .push(("wf_a".to_string(), script("a"), Value::Null));
    let service = RequestService::new(&registry, &runs, paths, cwd, true);
    let prepared = service
        .prepare(&request(json!({"source":{"type":"stop"}})))
        .expect("unique");
    assert!(matches!(
        prepared,
        PreparedRequest::Control {
            action: ControlAction::Stop,
            run_id,
        } if run_id == "wf_a"
    ));
}

#[test]
fn explicit_control_unknown_run_is_reported() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path());
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    let runs = MockRuns::default();
    let service = RequestService::new(&registry, &runs, paths, home.path().to_path_buf(), true);
    let error = service
        .prepare(&request(
            json!({"source":{"type":"pause","run_id":"wf_nope"}}),
        ))
        .expect_err("unknown run");
    assert_eq!(error.code(), "workflow_control_unknown_run");
}

#[test]
fn import_grok_is_explicit_validated_and_non_overwriting() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path());
    let grok = tempfile::tempdir().expect("grok");
    std::fs::write(grok.path().join("imported.rhai"), script("imported")).expect("write");
    std::fs::write(grok.path().join("wrong.rhai"), script("other")).expect("write");
    std::fs::write(
        grok.path().join("huge.rhai"),
        "x".repeat(MAX_WORKFLOW_SOURCE_BYTES),
    )
    .expect("write");
    std::fs::write(grok.path().join("existing.rhai"), script("existing")).expect("write");
    // Not called yet: a plain registry scan must not import anything.
    let _ = WorkflowRegistry::new(paths.clone()).expect("registry");
    assert!(!paths.user_dir().exists());

    std::fs::create_dir_all(paths.user_dir()).expect("user dir");
    std::fs::write(paths.user_dir().join("existing.rhai"), "user-owned").expect("preexisting");

    let report = import_grok(grok.path(), &paths).expect("import");
    assert_eq!(report.imported, ["imported"]);
    assert_eq!(report.skipped_existing, ["existing"]);
    let rejected: Vec<_> = report
        .rejected
        .iter()
        .map(|(file, _)| file.as_str())
        .collect();
    assert_eq!(rejected, ["huge.rhai", "wrong.rhai"]);
    assert_eq!(
        std::fs::read_to_string(paths.user_dir().join("existing.rhai")).unwrap(),
        "user-owned"
    );
    assert_eq!(
        std::fs::read_to_string(paths.user_dir().join("imported.rhai")).unwrap(),
        script("imported")
    );
}
