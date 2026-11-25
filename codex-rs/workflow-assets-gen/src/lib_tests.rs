//! Tests for workflow asset generation and verification.

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::*;

fn script(name: &str, body: &str) -> String {
    format!(
        "let meta = #{{\n    name: \"{name}\",\n    description: \"probe workflow\",\n    phases: [#{{ title: \"Only\" }}],\n}};\n{body}\n"
    )
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write fixture");
}

fn fixture() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let assets = dir.path();
    write(
        &assets.join("builtin").join("probe.rhai"),
        &script("probe", "complete(#{ ok: true });"),
    );
    write(
        &assets.join("bundled").join("probe-bundled.rhai"),
        &script("probe-bundled", "complete(#{ ok: true });"),
    );
    write(
        &assets
            .join("bundled")
            .join("prompts")
            .join("shared")
            .join("plan.md"),
        "Plan: {objective}\n",
    );
    write(
        &assets
            .join("bundled")
            .join("prompts")
            .join("shared")
            .join("report.md"),
        "Report: {objective}\n",
    );
    dir
}

#[test]
fn empty_directory_generates_empty_tables() {
    let dir = TempDir::new().expect("temp dir");
    let table = collect(dir.path()).expect("collect");
    assert!(table.builtin.is_empty());
    assert!(table.bundled.is_empty());
    assert!(table.prompts.is_empty());
    let source = generate(dir.path()).expect("generate");
    assert!(source.contains("BUILTIN_WORKFLOWS: &[EmbeddedAsset] = &["));
    assert!(source.contains("TEMPLATE_KEYS"));
}

#[test]
fn generation_is_byte_identical_across_runs_and_input_order() {
    let dir = fixture();
    let first = generate(dir.path()).expect("first");
    let second = generate(dir.path()).expect("second");
    assert_eq!(first, second);

    // Rewrite files in a different order; content-addressed output is stable.
    write(
        &dir.path()
            .join("bundled")
            .join("prompts")
            .join("shared")
            .join("report.md"),
        "Report: {objective}\n",
    );
    write(
        &dir.path().join("builtin").join("probe.rhai"),
        &script("probe", "complete(#{ ok: true });"),
    );
    let third = generate(dir.path()).expect("third");
    assert_eq!(first, third);
}

#[test]
fn assets_are_sorted_by_relative_path() {
    let dir = fixture();
    write(
        &dir.path().join("builtin").join("alpha.rhai"),
        &script("alpha", "complete(#{ ok: true });"),
    );
    write(
        &dir.path().join("builtin").join("zeta.rhai"),
        &script("zeta", "complete(#{ ok: true });"),
    );
    let table = collect(dir.path()).expect("collect");
    let paths: Vec<_> = table
        .builtin
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec![
            "builtin/alpha.rhai",
            "builtin/probe.rhai",
            "builtin/zeta.rhai"
        ]
    );
}

#[test]
fn duplicate_template_key_is_rejected() {
    let dir = fixture();
    write(
        &dir.path()
            .join("bundled")
            .join("prompts")
            .join("other")
            .join("plan.md"),
        "Other plan\n",
    );
    let error = collect(dir.path()).expect_err("duplicate key");
    match error {
        AssetError::DuplicateTemplateKey { key, first, second } => {
            assert_eq!(key, "plan");
            let sources = [first, second];
            assert!(sources.iter().any(|source| source.contains("shared")));
            assert!(sources.iter().any(|source| source.contains("other")));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn builtin_with_capability_mode_is_rejected() {
    let dir = fixture();
    write(
        &dir.path().join("builtin").join("probe.rhai"),
        &script(
            "probe",
            "let result = agent(\"do it\", #{ capability_mode: \"read-only\" });\ncomplete(result);",
        ),
    );
    let error = collect(dir.path()).expect_err("unsupported field");
    match error {
        AssetError::UnsupportedOptionField { field, line, .. } => {
            assert_eq!(field, "capability_mode");
            assert!(line.is_some());
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn bundled_scripts_may_keep_unsupported_fields_out_of_scope() {
    // Only builtin assets are statically rejected; bundled files are checked at
    // runtime. This keeps the generator's contract explicit.
    let dir = fixture();
    write(
        &dir.path().join("bundled").join("probe-bundled.rhai"),
        &script(
            "probe-bundled",
            "let result = agent(\"do it\", #{ capability_mode: \"read-only\" });\ncomplete(result);",
        ),
    );
    let table = collect(dir.path()).expect("bundled is not statically rejected");
    assert_eq!(table.bundled.len(), 1);
}

#[test]
fn invalid_script_is_rejected() {
    let dir = fixture();
    write(
        &dir.path().join("builtin").join("probe.rhai"),
        "let meta = #{ name: \"probe\" }; this is not rhai {{{\n",
    );
    let error = collect(dir.path()).expect_err("invalid script");
    assert!(matches!(error, AssetError::InvalidScript { .. }));
}

#[test]
fn file_name_must_match_meta_name() {
    let dir = fixture();
    write(
        &dir.path().join("builtin").join("probe.rhai"),
        &script("other", "complete(#{ ok: true });"),
    );
    let error = collect(dir.path()).expect_err("name mismatch");
    match error {
        AssetError::NameMismatch {
            file_stem,
            meta_name,
            ..
        } => {
            assert_eq!(file_stem, "probe");
            assert_eq!(meta_name, "other");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn carriage_returns_are_rejected() {
    let dir = fixture();
    write(
        &dir.path().join("builtin").join("probe.rhai"),
        &script("probe", "complete(#{ ok: true });").replace('\n', "\r\n"),
    );
    let error = collect(dir.path()).expect_err("crlf");
    assert!(matches!(error, AssetError::CarriageReturn { .. }));
}

#[test]
fn oversized_asset_is_rejected() {
    let dir = TempDir::new().expect("temp dir");
    let big = format!("{}\n", "x".repeat(MAX_WORKFLOW_SCRIPT_BYTES + 1));
    write(&dir.path().join("builtin").join("probe.rhai"), &big);
    let error = collect(dir.path()).expect_err("too large");
    assert!(matches!(error, AssetError::TooLarge { .. }));
}

#[test]
fn check_detects_stale_output_and_accepts_fresh_output() {
    let dir = fixture();
    let generated = dir.path().join("generated.rs");
    fs::write(&generated, generate(dir.path()).expect("generate")).expect("write generated");
    check(dir.path(), &generated).expect("fresh output passes");

    fs::write(&generated, "// changed\n").expect("tamper");
    let error = check(dir.path(), &generated).expect_err("stale output");
    assert!(matches!(error, AssetError::Stale { .. }));
}

#[test]
fn generated_source_embeds_sha256_and_template_lookup() {
    let dir = fixture();
    let source = generate(dir.path()).expect("generate");
    let expected_hash = sha256_hex(b"Plan: {objective}\n");
    assert!(source.contains(&expected_hash));
    assert!(source.contains("match name {"));
    assert!(source.contains("\"plan\" => Some("));
    assert!(!source.contains("capability_mode"));
}
