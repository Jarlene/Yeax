//! F1..F31 failure-injection matrix for the workflow runtime (TASK-040).
//!
//! Cases that only need the engine, the host protocol, or a validation helper
//! run here. Cases that need a real model turn, a live session, crash injection,
//! Bazel, or Windows stay as ignored carriers; `evidence/E2E-F.md` records the
//! per-case status.

use serde_json::json;

use codex_core::workflow::testing::builtin_names;
use codex_core::workflow::testing::bundled_template_keys;
use codex_core::workflow::testing::is_valid_segment;
use codex_core::workflow::testing::run_script_with_mock_agents;
use codex_core::workflow::testing::validate_output_schema;
use codex_core::workflow::testing::validate_schema_text;
use codex_core::workflow::testing::validate_script;

const HEADER: &str = "let meta = #{ name: \"%NAME%\", description: \"d\", when_to_use: \"t\", phases: [#{ title: \"Run\", detail: \"d\" }] };\n";

fn script(body: &str) -> String {
    format!("{}{body}", HEADER.replace("%NAME%", "matrix"))
}

#[test]
fn f1_missing_name_is_rejected() {
    let error = validate_script("let meta = #{ description: \"d\" };").expect_err("meta");
    assert!(error.to_lowercase().contains("name"), "{error}");
}

#[test]
fn f2_name_with_slash_is_rejected() {
    let bad = HEADER.replace("%NAME%", "bad/name");
    assert!(validate_script(&bad).is_err());
}

#[test]
fn f3_too_many_phases_are_rejected() {
    let phases = (0..65)
        .map(|index| format!("#{{ title: \"P{index}\", detail: \"d\" }}"))
        .collect::<Vec<_>>()
        .join(", ");
    let bad = format!(
        "let meta = #{{ name: \"matrix\", description: \"d\", when_to_use: \"t\", phases: [{phases}] }};"
    );
    assert!(validate_script(&bad).is_err());
}

#[test]
#[ignore = "registry path: oversized sources are covered by registry_tests"]
fn f4_oversized_script_is_rejected() {}

#[test]
fn f5_external_ref_is_rejected() {
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "$ref": "https://example.com/answer.json" } }
    });
    let error = validate_output_schema(schema).expect_err("external ref");
    assert!(error.to_lowercase().contains("ref"), "{error}");
}

#[test]
fn f6_plain_text_fails_an_object_schema() {
    let schema = json!({
        "type": "object",
        "required": ["answer"],
        "properties": { "answer": { "type": "string" } }
    });
    assert!(validate_schema_text(schema, "just prose, no JSON").is_err());
}

#[tokio::test]
async fn f7_unknown_nested_workflow_fails_the_run() {
    let script = script("workflow(\"does-not-exist\");\ncomplete(#{ ok: true });");
    let report = run_script_with_mock_agents(&script, json!({}), Vec::new())
        .await
        .expect("engine run");
    assert!(!report.completed);
    let error = report.error.unwrap_or_default();
    assert!(error.contains("does-not-exist"), "{error}");
}

#[test]
#[ignore = "requires a real child: a workflow child calling the workflow tool needs host wiring"]
fn f8_child_agent_cannot_start_a_workflow() {}

#[test]
#[ignore = "host_service_tests covers unsupported agent opts before spawning"]
fn f9a_unsupported_agent_option_fails_before_spawn() {}

#[test]
#[ignore = "requires Bazel: builtin-source static check (D-MIG-001) is a Bazel rust_test"]
fn f9b_builtin_static_check() {}

#[test]
fn f10_scratch_name_escape_is_rejected() {
    assert!(!is_valid_segment("../escape"));
    assert!(!is_valid_segment("nested/file"));
    assert!(is_valid_segment("report"));
}

#[test]
#[ignore = "store/persist_tests cover the 65-file and 10MB scratch limits"]
fn f11_scratch_limits() {}

#[test]
#[ignore = "engine budget tests cover the 2049-spawn cap"]
fn f12_spawn_cap() {}

#[test]
#[ignore = "requires real children: 2049 live child agents"]
fn f13_live_child_cap() {}

#[test]
#[ignore = "run_manager stop-idempotency covers this without a live session"]
fn f14_stop_immediately_after_spawn() {}

#[test]
#[ignore = "requires the host child spawn path, not the mock spawner"]
fn f15_child_returns_immediately() {}

#[test]
#[ignore = "protocol/app-server tests cover unknown-notification compatibility"]
fn f16_old_client_unknown_notification() {}

#[test]
fn f17_schema_repair_accepts_a_valid_second_answer() {
    let schema = json!({
        "type": "object",
        "required": ["answer"],
        "properties": { "answer": { "type": "string" } }
    });
    assert!(validate_schema_text(schema.clone(), "not json").is_err());
    let validated = validate_schema_text(schema, "{\"answer\":\"fixed\"}").expect("second answer");
    assert_eq!(validated["answer"], json!("fixed"));
}

#[test]
fn f18_two_invalid_answers_keep_failing() {
    let schema = json!({
        "type": "object",
        "required": ["answer"],
        "properties": { "answer": { "type": "string" } }
    });
    assert!(validate_schema_text(schema.clone(), "no").is_err());
    assert!(validate_schema_text(schema, "{\"answer\":42}").is_err());
}

#[test]
#[ignore = "TUI Start still goes through a user turn; see docs/workflow-migration-consistency.md F-02"]
fn f19_direct_workflow_without_turn() {}

#[test]
#[ignore = "requires real AgentControl::send error injection"]
fn f20_send_error() {}

#[test]
#[ignore = "admission_tests cover permit release; probe wiring is in workflow/runtime.rs"]
fn f21_ownership_without_admission() {}

#[test]
#[ignore = "outcome_hub_tests cover the tombstone suppression after cancellation"]
fn f22_late_terminal_after_cancel() {}

#[test]
#[ignore = "outcome_hub_tests cover drain and tombstone TTL"]
fn f23_child_never_reports_terminal() {}

#[test]
#[ignore = "requires real children: 33 turns in one run"]
fn f24_many_turns_in_one_run() {}

#[test]
#[ignore = "requires a direct /workflow launch path (F-02)"]
fn f25_forced_field_override() {}

#[test]
#[ignore = "persist_tests cover create-only concurrent writes"]
fn f26_concurrent_create_only() {}

#[test]
#[ignore = "requires crash injection before publish"]
fn f27_crash_before_publish() {}

#[test]
#[ignore = "store_tests cover concurrent manifest/state updates and revision monotonicity"]
fn f28_manifest_state_race() {}

#[test]
#[ignore = "requires a crash-injection driver around rename"]
fn f29_crash_around_rename() {}

#[test]
#[ignore = "engine journal tests cover a half-line tail; disk-full needs a fault FS"]
fn f30_half_journal_line_and_disk_full() {}

#[test]
fn f31a_bundled_assets_are_present() {
    let names = builtin_names();
    assert!(names.contains(&"hello-workflow"), "{names:?}");
    assert!(names.contains(&"deep-research"), "{names:?}");
}

#[test]
fn f31b_prompt_templates_are_present_and_unique() {
    let keys = bundled_template_keys();
    for expected in ["plan", "report", "research", "verify"] {
        assert!(keys.contains(&expected), "missing {expected}: {keys:?}");
    }
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), keys.len(), "duplicate template key: {keys:?}");
}

#[test]
#[ignore = "requires Bazel: F31d is a build-time asset failure"]
fn f31d_build_time_asset_failure() {}
