//! Tests for workflow metadata validation.

use super::*;

#[test]
fn extracts_valid_meta() {
    let meta = extract_meta(
        r#"
            // A workflow.
            let meta = #{
                name: "demo",
                description: "does things",
                phases: [#{ title: "Scan" }, #{ title: "Fix", detail: "apply" }],
            };
            let x = agent("hi");
            "#,
    )
    .expect("valid meta");
    assert_eq!(meta.name, "demo");
    assert_eq!(meta.phases.len(), 2);
    assert_eq!(
        meta.phases.get(1).and_then(|p| p.detail.as_deref()),
        Some("apply")
    );
}

#[test]
fn rejects_missing_meta() {
    assert!(matches!(
        extract_meta("let x = 1;"),
        Err(MetaError::MetaNotFirst)
    ));
}

#[test]
fn rejects_meta_not_first() {
    assert!(matches!(
        extract_meta(r#"let x = 1; let meta = #{ name: "n", description: "d" };"#),
        Err(MetaError::MetaNotFirst)
    ));
}

#[test]
fn rejects_empty_name() {
    assert!(matches!(
        extract_meta(r#"let meta = #{ name: "", description: "d" };"#),
        Err(MetaError::MissingField("meta.name"))
    ));
}

#[test]
fn rejects_non_kebab_case_names() {
    for name in [
        "Upper",
        "under_score",
        "-leading",
        "trailing-",
        "two--hyphens",
        "-1",
    ] {
        let script = format!(r#"let meta = #{{ name: "{name}", description: "d" }};"#);
        assert!(
            matches!(extract_meta(&script), Err(MetaError::InvalidName)),
            "accepted invalid name {name:?}"
        );
    }
}

#[test]
fn accepts_name_bounds() {
    let name = format!("1{}", "a".repeat(MAX_WORKFLOW_NAME_LEN - 1));
    let script = format!(r#"let meta = #{{ name: "{name}", description: "d" }};"#);
    assert_eq!(extract_meta(&script).unwrap().name, name);
}

#[test]
fn rejects_oversized_meta_strings_and_phases() {
    let name = "a".repeat(MAX_WORKFLOW_NAME_LEN + 1);
    let script = format!(r#"let meta = #{{ name: "{name}", description: "d" }};"#);
    assert!(matches!(
        extract_meta(&script),
        Err(MetaError::StringTooLong { field, .. }) if field == "meta.name"
    ));

    let phases = std::iter::repeat_n(r#"#{ title: "phase" }"#, MAX_WORKFLOW_PHASES + 1)
        .collect::<Vec<_>>()
        .join(",");
    let script =
        format!(r#"let meta = #{{ name: "valid", description: "d", phases: [{phases}] }};"#);
    assert!(matches!(
        extract_meta(&script),
        Err(MetaError::TooManyPhases { .. })
    ));
}

#[test]
fn rejects_empty_phase_title() {
    let error = extract_meta(
        r#"let meta = #{ name: "valid", description: "d", phases: [#{ title: " " }] };"#,
    );
    assert!(matches!(
        error,
        Err(MetaError::MissingField("meta.phases[].title"))
    ));
}

#[test]
fn rejects_duplicate_phase_titles() {
    let error = extract_meta(
        r#"let meta = #{ name: "valid", description: "d", phases: [#{ title: "Scan" }, #{ title: "Scan" }] };"#,
    );
    assert!(matches!(
        error,
        Err(MetaError::InvalidShape(message)) if message.contains("duplicate meta.phases[].title")
    ));
}

#[test]
fn rejects_unknown_meta_fields() {
    assert!(matches!(
        extract_meta(r#"let meta = #{ name: "valid", description: "d", typo: "ignored?" };"#,),
        Err(MetaError::InvalidShape(_))
    ));
    assert!(matches!(
        extract_meta(
            r#"let meta = #{ name: "valid", description: "d", phases: [#{ title: "p", typo: true }] };"#,
        ),
        Err(MetaError::InvalidShape(_))
    ));
}

#[test]
fn rejects_syntax_errors() {
    assert!(matches!(
        extract_meta(r#"let meta = #{ name: "n", description: "d" }; fn {"#),
        Err(MetaError::Parse(_))
    ));
}

#[test]
fn comments_before_meta_are_fine() {
    let meta = extract_meta(
        "/* header\ncomment */\n// line\nlet meta = #{ name: \"n\", description: \"d\" };",
    );
    assert!(meta.is_ok());
}
