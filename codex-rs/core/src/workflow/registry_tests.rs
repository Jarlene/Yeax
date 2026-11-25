//! Tests for the workflow registry.

use std::path::Path;
use std::sync::Arc;

use codex_workflow::extract_meta;

use super::*;

fn script(name: &str) -> String {
    format!("let meta = #{{ name: \"{name}\", description: \"d\" }};\ncomplete(\"ok\");")
}

fn paths(home: &Path, git_root: Option<&Path>, trusted: bool) -> Arc<WorkflowPaths> {
    Arc::new(
        WorkflowPaths::resolve_with_git_root(home.to_path_buf(), git_root, "sess-1", None)
            .expect("paths")
            .with_project_trusted(trusted),
    )
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("dir");
    std::fs::write(dir.join(name), body).expect("write");
}

#[test]
fn empty_home_still_exposes_builtins_without_writing() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");

    let names = registry.available_names();
    assert!(names.contains(&"deep-research".to_string()));
    assert!(names.contains(&"hello-workflow".to_string()));
    let resolved = registry.resolve_by_name("deep-research").expect("resolve");
    assert_eq!(resolved.scope, Some(WorkflowScope::Builtin));
    assert_eq!(resolved.meta.phases.len(), 4);
    assert!(!paths.user_dir().exists());
    assert!(!paths.bundled_dir().exists());
}

#[test]
fn missing_or_corrupt_bundled_index_is_stable() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    write(
        paths.bundled_dir(),
        "hello-bundled.rhai",
        &script("hello-bundled"),
    );
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry without index");
    assert_eq!(
        registry.resolve_by_name("hello-bundled").unwrap().scope,
        Some(WorkflowScope::Bundled)
    );

    std::fs::write(paths.bundled_index_path(), b"{not-json").expect("corrupt index");
    let registry = WorkflowRegistry::new(paths).expect("registry with corrupt index");
    assert_eq!(
        registry.resolve_by_name("hello-bundled").unwrap().scope,
        Some(WorkflowScope::Bundled)
    );
}

#[test]
fn bundled_shadows_builtin_by_name() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    let bundled_body =
        "let meta = #{ name: \"deep-research\", description: \"from-bundle\" };\ncomplete(\"ok\");";
    write(paths.bundled_dir(), "deep-research.rhai", bundled_body);

    let registry = WorkflowRegistry::new(paths).expect("registry");
    let resolved = registry.resolve_by_name("deep-research").expect("resolve");
    assert_eq!(resolved.scope, Some(WorkflowScope::Bundled));
    assert_eq!(resolved.meta.description, "from-bundle");
    let listing = registry
        .list()
        .into_iter()
        .find(|entry| entry.name == "deep-research")
        .expect("listing");
    assert!(listing.shadowed_by.is_none());
}

#[test]
fn priority_is_bundled_builtin_project_user_with_candidate_injection() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    let meta = extract_meta(&script("same")).expect("meta");
    let registry = WorkflowRegistry::from_candidates(vec![
        (
            WorkflowScope::Bundled,
            vec![(meta.clone(), script("same"), "bundled/same.rhai".into())],
        ),
        (
            WorkflowScope::Builtin,
            vec![(meta.clone(), script("same"), "builtin/same.rhai".into())],
        ),
        (
            WorkflowScope::Project,
            vec![(meta.clone(), script("same"), "project/same.rhai".into())],
        ),
        (
            WorkflowScope::User,
            vec![(meta, script("same"), "user/same.rhai".into())],
        ),
    ]);
    assert_eq!(
        registry.resolve_by_name("same").unwrap().scope,
        Some(WorkflowScope::Bundled)
    );
    let shadowed: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|entry| entry.shadowed_by == Some("bundled"))
        .map(|entry| entry.scope)
        .collect();
    assert_eq!(shadowed, ["builtin", "project", "user"]);
    let _ = paths;
}

#[test]
fn same_scope_duplicate_is_rejected() {
    let meta = extract_meta(&script("dup")).expect("meta");
    let registry = WorkflowRegistry::from_candidates(vec![(
        WorkflowScope::Project,
        vec![
            (meta.clone(), script("dup"), "a/dup.rhai".into()),
            (meta, script("dup"), "b/dup.rhai".into()),
        ],
    )]);
    let error = registry.resolve_by_name("dup").expect_err("duplicate");
    assert!(matches!(
        error,
        RegistryError::DuplicateName {
            scope: WorkflowScope::Project,
            ..
        }
    ));
}

#[test]
fn filename_must_match_meta_name() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    write(paths.user_dir(), "wrong.rhai", &script("other"));
    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    assert!(
        registry
            .available_names()
            .iter()
            .all(|name| name != "other")
    );
    assert!(matches!(
        resolve_by_path(&paths.user_dir().join("wrong.rhai"), home.path(), &paths),
        Err(RegistryError::FilenameMismatch { .. })
    ));
}

#[test]
fn oversized_and_invalid_names_are_rejected() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    std::fs::create_dir_all(paths.user_dir()).expect("dir");
    std::fs::write(
        paths.user_dir().join("huge.rhai"),
        "x".repeat(MAX_WORKFLOW_SOURCE_BYTES + 1),
    )
    .expect("write");
    let registry = WorkflowRegistry::new(paths).expect("registry");
    assert!(registry.available_names().iter().all(|name| name != "huge"));

    for name in [
        "", "..", "a/b", "a\\b", "../x", "UPPER", "-lead", "trail-", "a--b",
    ] {
        assert!(matches!(
            validate_name(name),
            Err(RegistryError::InvalidName(_))
        ));
    }
}

#[test]
fn user_save_is_create_only_and_byte_stable() {
    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    let body = script("saved");
    let target = save_user_workflow(&paths, "saved", &body).expect("save");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), body);

    let error = save_user_workflow(&paths, "saved", &script("saved")).expect_err("exists");
    assert_eq!(error.code(), "workflow_save_exists");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), body);

    assert!(matches!(
        save_user_workflow(&paths, "../escape", &body),
        Err(RegistryError::InvalidName(_))
    ));
    assert!(matches!(
        save_user_workflow(&paths, "filename", &script("different")),
        Err(RegistryError::FilenameMismatch { .. })
    ));
}

#[test]
fn project_scope_is_trust_gated_and_confined() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let script_body = script("project-only");
    write(
        &project.path().join(".codex/workflows"),
        "project-only.rhai",
        &script_body,
    );

    let untrusted = paths(home.path(), Some(project.path()), false);
    let registry = WorkflowRegistry::new(untrusted.clone()).expect("registry");
    assert!(
        registry
            .available_names()
            .iter()
            .all(|name| name != "project-only")
    );
    assert!(matches!(
        resolve_by_path(
            &project.path().join(".codex/workflows/project-only.rhai"),
            project.path(),
            &untrusted,
        ),
        Err(RegistryError::UntrustedPath { .. })
    ));
    assert!(matches!(
        save_project_workflow(&untrusted, "saved", &script("saved")),
        Err(RegistryError::UntrustedPath { .. })
    ));

    let trusted = paths(home.path(), Some(project.path()), true);
    let registry = WorkflowRegistry::new(trusted.clone()).expect("registry");
    assert_eq!(
        registry.resolve_by_name("project-only").unwrap().scope,
        Some(WorkflowScope::Project)
    );
    let target = save_project_workflow(&trusted, "saved", &script("saved")).expect("save");
    assert!(target.starts_with(project.path()));
}

#[test]
fn explicit_path_outside_trusted_roots_is_rejected() {
    let home = tempfile::tempdir().expect("home");
    let project = tempfile::tempdir().expect("project");
    let outside = tempfile::tempdir().expect("outside");
    write(outside.path(), "demo.rhai", &script("demo"));

    let untrusted = paths(home.path(), Some(project.path()), false);
    let error = resolve_by_path(
        &outside.path().join("demo.rhai"),
        project.path(),
        &untrusted,
    )
    .expect_err("outside");
    assert!(matches!(error, RegistryError::UntrustedPath { .. }));
}

#[cfg(unix)]
#[test]
fn symlinked_workflow_is_rejected_and_not_listed() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("home");
    let paths = paths(home.path(), None, false);
    let target = home.path().join("real.rhai");
    std::fs::write(&target, script("linked")).expect("target");
    std::fs::create_dir_all(paths.user_dir()).expect("dir");
    symlink(&target, paths.user_dir().join("linked.rhai")).expect("link");

    let registry = WorkflowRegistry::new(paths.clone()).expect("registry");
    assert!(
        registry
            .available_names()
            .iter()
            .all(|name| name != "linked")
    );
    let error = resolve_by_path(&paths.user_dir().join("linked.rhai"), home.path(), &paths)
        .expect_err("symlink");
    assert_eq!(error.code(), "workflow_script_symlink_rejected");
}
