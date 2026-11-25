//! Build script that verifies `src/workflow/assets/generated.rs` is up to
//! date with the checked-in workflow and prompt sources.

use std::path::PathBuf;

/// Fail the build when `src/workflow/assets/generated.rs` no longer matches
/// the checked-in `.rhai` / prompt sources. Regenerating is deterministic:
/// `cargo run -p codex-workflow-assets-gen -- --write`.
fn main() {
    // `admission.rs` uses a `standalone_logic` cfg only when the module is
    // compiled outside the real core crate (standalone harness).
    println!("cargo::rustc-check-cfg=cfg(standalone_logic)");

    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(error) => panic!("cargo always sets CARGO_MANIFEST_DIR: {error}"),
    };
    let assets_dir = manifest_dir.join("src").join("workflow").join("assets");
    let generated = assets_dir.join("generated.rs");

    println!("cargo:rerun-if-changed={}", assets_dir.display());
    println!("cargo:rerun-if-changed={}", generated.display());

    if let Err(error) = codex_workflow_assets_gen::check(&assets_dir, &generated) {
        panic!(
            "workflow asset check failed: {error}\n\
             generated.rs is out of date; run \
             `cargo run -p codex-workflow-assets-gen -- --write` and commit the result"
        );
    }
}
