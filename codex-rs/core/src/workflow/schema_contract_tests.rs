//! Tests for workflow schema contracts.

use serde_json::json;

use super::*;

fn contract() -> SchemaContract {
    SchemaContract::parse(json!({
        "type": "object",
        "required": ["ok"],
        "properties": {"ok": {"type": "boolean"}},
        "additionalProperties": false
    }))
    .expect("schema")
}

#[test]
fn valid_output_passes() {
    assert_eq!(
        contract().validate_text("{\"ok\":true}").expect("valid"),
        json!({"ok": true})
    );
}

#[test]
fn invalid_output_reports_validation_errors() {
    let error = contract()
        .validate_text("{\"ok\":\"yes\"}")
        .expect_err("invalid");
    match error {
        SchemaError::ValidationFailed(errors) => assert!(!errors.is_empty()),
        other => panic!("unexpected: {other}"),
    }
}

#[test]
fn fenced_json_is_preferred() {
    let text = "Here you go:\n```json\n{\n  \"ok\": true\n}\n```\nThanks";
    assert_eq!(
        extract_json_candidate(text).expect("candidate"),
        "{\n  \"ok\": true\n}"
    );
    assert_eq!(
        contract().validate_text(text).expect("valid"),
        json!({"ok": true})
    );
}

#[test]
fn balanced_json_survives_prose_and_braces_in_strings() {
    let text = "prefix {\"ok\": true, \"note\": \"a } inside a string\"} suffix";
    assert_eq!(
        extract_json_candidate(text).expect("candidate"),
        "{\"ok\": true, \"note\": \"a } inside a string\"}"
    );
}

#[test]
fn missing_json_is_reported() {
    assert!(matches!(
        contract()
            .validate_text("no json here")
            .expect_err("missing"),
        SchemaError::MissingJson
    ));
}

#[test]
fn external_refs_are_rejected() {
    let error = SchemaContract::parse(json!({
        "$ref": "https://example.com/schema.json"
    }))
    .expect_err("external ref");
    assert!(matches!(error, SchemaError::InvalidSchema(_)));
}

#[test]
fn internal_refs_are_allowed() {
    SchemaContract::parse(json!({
        "type": "object",
        "properties": {"child": {"$ref": "#/definitions/x"}},
        "definitions": {"x": {"type": "string"}}
    }))
    .expect("internal ref");
}

#[test]
fn non_object_root_is_rejected() {
    assert!(SchemaContract::parse(json!(42)).is_err());
}

#[test]
fn prompt_suffix_carries_the_canonical_schema() {
    let suffix = contract().prompt_suffix();
    assert!(suffix.starts_with(OUTPUT_CONTRACT_OPEN));
    assert!(suffix.ends_with(OUTPUT_CONTRACT_CLOSE));
    assert!(suffix.contains("\"required\":[\"ok\"]"));
}

#[test]
fn retry_prompt_lists_errors_and_repeats_the_contract() {
    let prompt = contract().retry_prompt(&["property ok must be boolean".to_string()]);
    assert!(prompt.contains("property ok must be boolean"));
    assert!(prompt.contains(OUTPUT_CONTRACT_OPEN));
}

#[test]
fn only_one_retry_attempt_is_allowed() {
    assert_eq!(MAX_SCHEMA_ATTEMPTS, 2);
}

#[test]
fn oversized_schema_is_rejected() {
    let huge = json!({"description": "x".repeat(MAX_SCHEMA_BYTES)});
    assert!(matches!(
        SchemaContract::parse(huge).expect_err("too large"),
        SchemaError::TooLarge { .. }
    ));
}
