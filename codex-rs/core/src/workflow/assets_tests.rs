//! Tests for embedded workflow assets.

use std::fs;

use serde_json::json;
use tempfile::TempDir;

use super::*;

#[test]
fn builtin_assets_carry_deep_research_and_hello() {
    let names: Vec<_> = builtin_scripts().iter().map(|asset| asset.name).collect();
    assert_eq!(names, vec!["deep-research", "hello-workflow"]);
}

#[test]
fn builtin_deep_research_has_no_capability_mode() {
    let asset = builtin_scripts()
        .iter()
        .find(|asset| asset.name == "deep-research")
        .expect("deep-research");
    assert!(!asset.content.contains("capability_mode"));
    assert_eq!(asset.content.matches("phase(\"").count(), 4);
}

#[test]
fn bundled_assets_match_the_design_contract() {
    let names: Vec<_> = bundled_scripts().iter().map(|asset| asset.name).collect();
    assert_eq!(names, vec!["hello-bundled"]);
    assert_eq!(template_keys(), &["plan", "report", "research", "verify"]);
}

#[test]
fn embedded_sha256_matches_content() {
    for asset in builtin_scripts()
        .iter()
        .chain(bundled_scripts())
        .chain(bundled_prompts())
    {
        assert_eq!(
            sha256_hex(asset.content.as_bytes()),
            asset.sha256,
            "{} hash mismatch",
            asset.path
        );
    }
}

#[test]
fn render_replaces_known_keys_and_keeps_unknown_ones() {
    let rendered =
        render_template("research", &json!({"objective": "ship it", "x": 3})).expect("render");
    assert!(rendered.contains("ship it"));
    assert!(rendered.contains("{question}"));
    assert!(!rendered.contains("{objective}"));
}

#[test]
fn render_supports_nested_keys_and_compact_values() {
    let rendered = render_template_source(
        "v={a.b} n={n} b={b} o={o} missing={nope} bad={a b}",
        &json!({"a": {"b": "deep"}, "n": 7, "b": true, "o": {"k": 1}}),
    )
    .expect("render");
    assert_eq!(
        rendered,
        "v=deep n=7 b=true o={\"k\":1} missing={nope} bad={a b}"
    );
}

#[test]
fn unknown_template_error_text_is_stable() {
    let error = render_template("missing", &json!({})).expect_err("unknown");
    assert_eq!(error.to_string(), "unknown template: missing");
}

#[test]
fn bundled_install_is_create_only_then_up_to_date() {
    let dir = TempDir::new().expect("temp");
    let bundled = dir.path().join("bundled").join("workflows");
    let first = ensure_bundled_assets(&bundled).expect("install");
    assert_eq!(first.installed, 5);
    assert_eq!(first.upgraded, 0);
    assert!(bundled.join("hello-bundled.rhai").is_file());
    assert!(
        bundled
            .join("prompts")
            .join("shared")
            .join("plan.md")
            .is_file()
    );
    assert!(bundled.join(".bundled-index.json").is_file());

    let second = ensure_bundled_assets(&bundled).expect("second install");
    assert_eq!(second.installed, 0);
    assert_eq!(second.up_to_date, 5);
    assert!(!bundled.join(".bundled-index.lock").exists());
}

#[test]
fn user_modified_bundled_asset_is_preserved() {
    let dir = TempDir::new().expect("temp");
    let bundled = dir.path().join("bundled");
    ensure_bundled_assets(&bundled).expect("install");
    let target = bundled.join("hello-bundled.rhai");
    fs::write(&target, "user edit").expect("edit");
    let report = ensure_bundled_assets(&bundled).expect("install again");
    assert_eq!(report.skipped_modified, 1);
    assert_eq!(fs::read_to_string(&target).expect("read"), "user edit");
    assert_eq!(report.warnings.len(), 1);
}

#[test]
fn indexed_asset_is_upgraded_atomically() {
    let dir = TempDir::new().expect("temp");
    let bundled = dir.path().join("bundled");
    ensure_bundled_assets(&bundled).expect("install");
    let target = bundled.join("hello-bundled.rhai");
    // Simulate a previous release's content that matches the recorded hash.
    let old = "let meta = #{ name: \"hello-bundled\", description: \"old\" };";
    fs::write(&target, old).expect("write old");
    let index_path = bundled.join(".bundled-index.json");
    let mut index: serde_json::Value =
        serde_json::from_slice(&fs::read(&index_path).expect("read")).expect("json");
    index["entries"]["hello-bundled.rhai"] = json!(sha256_hex(old.as_bytes()));
    fs::write(
        &index_path,
        serde_json::to_vec_pretty(&index).expect("json"),
    )
    .expect("write");

    let report = ensure_bundled_assets(&bundled).expect("upgrade");
    assert_eq!(report.upgraded, 1);
    assert_eq!(
        fs::read_to_string(&target).expect("read"),
        bundled_scripts()[0].content
    );
}

#[test]
fn corrupt_index_is_reported() {
    let dir = TempDir::new().expect("temp");
    let bundled = dir.path().join("bundled");
    fs::create_dir_all(&bundled).expect("mkdir");
    fs::write(bundled.join(".bundled-index.json"), b"{not json").expect("write");
    let error = ensure_bundled_assets(&bundled).expect_err("invalid index");
    assert!(matches!(error, InstallError::InvalidIndex { .. }));
}

#[test]
fn template_files_ship_with_documented_placeholders() {
    for asset in bundled_prompts() {
        assert!(asset.content.contains("{objective}"), "{}", asset.path);
        assert!(!asset.path.contains(".."));
    }
}
