//! Guards the checked-in workflow asset table against silent drift.

use std::path::PathBuf;

#[test]
fn checked_in_assets_match_the_generator() {
    let assets = codex_workflow_assets_gen::default_assets_dir();
    let generated = codex_workflow_assets_gen::default_generated_path();
    if let Err(error) = codex_workflow_assets_gen::check(&assets, &generated) {
        panic!(
            "workflow assets drifted: {error}\nrun `cargo run -p codex-workflow-assets-gen -- --write`"
        );
    }
}

#[test]
fn tampered_generated_file_fails_the_check() {
    let source = codex_workflow_assets_gen::default_assets_dir();
    let temp = tempfile::tempdir().expect("temp");
    copy_tree(&source, temp.path()).expect("copy asset tree");
    let generated = temp.path().join("generated.rs");
    let mut bytes = std::fs::read(&generated).expect("generated");
    bytes.extend_from_slice(b"\n// drift\n");
    std::fs::write(&generated, bytes).expect("tamper");

    let error = codex_workflow_assets_gen::check(temp.path(), &generated)
        .expect_err("tampered output must fail");
    assert!(matches!(
        error,
        codex_workflow_assets_gen::AssetError::Stale { .. }
    ));
}

#[test]
fn missing_assets_directory_fails_the_check() {
    let temp = tempfile::tempdir().expect("temp");
    let error = codex_workflow_assets_gen::check(temp.path(), &temp.path().join("generated.rs"))
        .expect_err("missing assets must fail");
    assert!(matches!(
        error,
        codex_workflow_assets_gen::AssetError::Io { .. }
    ));
}

fn copy_tree(source: &PathBuf, target: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &destination)?;
        } else {
            std::fs::copy(&path, &destination)?;
        }
    }
    Ok(())
}
