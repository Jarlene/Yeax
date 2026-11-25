//! Tests for the workflow run store.

use std::fs;

use tempfile::TempDir;

use super::*;

const SCRIPT: &str = "let meta = #{ name: \"probe\", description: \"probe\" };\ncomplete(());\n";

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("temp");
    let store = Store::new(dir.path().join("workflows").join("sess-1"));
    (dir, store)
}

#[test]
fn missing_manifest_is_empty_and_not_recovered() {
    let (_dir, store) = store();
    let load = store.load_manifest().expect("load");
    assert!(!load.recovered);
    assert!(load.manifest.runs.is_empty());
    assert_eq!(load.manifest.version, 4);
}

#[test]
fn create_run_freezes_files_and_registers_manifest() {
    let (_dir, store) = store();
    let layout = store
        .create_run(
            "wf_1",
            "probe",
            "objective",
            SCRIPT,
            "{\"query\":\"x\"}",
            Some("high"),
            10,
        )
        .expect("create");
    assert_eq!(
        fs::read_to_string(layout.script_path()).expect("script"),
        SCRIPT
    );
    assert_eq!(
        fs::read_to_string(layout.args_path()).expect("args"),
        "{\"query\":\"x\"}"
    );
    assert_eq!(
        fs::read_to_string(layout.effort_path()).expect("effort"),
        "high"
    );
    assert!(layout.scratch_dir().is_dir());

    let load = store.load_manifest().expect("load");
    assert_eq!(load.manifest.runs.len(), 1);
    let entry = &load.manifest.runs[0];
    assert_eq!(entry.run_id, "wf_1");
    assert_eq!(entry.status, RunStatus::Active);
    assert_eq!(entry.objective, "objective");
    assert_eq!(entry.created_at_ms, 10);
}

#[test]
fn duplicate_run_id_cannot_overwrite_frozen_script() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "objective", SCRIPT, "{}", None, 10)
        .expect("create");
    let error = store
        .create_run("wf_1", "probe", "objective", "different", "{}", None, 11)
        .expect_err("must not overwrite");
    assert!(matches!(
        error,
        StoreError::Persist(PersistError::Exists(_))
    ));
    let layout = store.run_layout("wf_1").expect("layout");
    assert_eq!(
        fs::read_to_string(layout.script_path()).expect("script"),
        SCRIPT
    );
}

#[test]
fn run_ids_are_validated_before_path_join() {
    let (_dir, store) = store();
    for run_id in ["", "..", "a/b", "a\\b"] {
        assert!(
            store.run_layout(run_id).is_err(),
            "{run_id:?} must be rejected"
        );
    }
}

#[test]
fn display_names_are_unique_within_a_session() {
    let (_dir, store) = store();
    store
        .create_run("wf_abc", "probe", "one", SCRIPT, "{}", None, 1)
        .expect("create");
    store
        .create_run("wf_def", "probe", "two", SCRIPT, "{}", None, 2)
        .expect("create");
    store
        .create_run("wf_ghi", "probe", "three", SCRIPT, "{}", None, 3)
        .expect("create");
    let load = store.load_manifest().expect("load");
    let names: Vec<_> = load
        .manifest
        .runs
        .iter()
        .map(|entry| entry.display_name.as_str())
        .collect();
    assert_eq!(names, vec!["probe", "probe-2", "probe-3"]);
}

#[test]
fn state_round_trips_atomically() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "objective", SCRIPT, "{}", None, 1)
        .expect("create");
    let mut state = RunState::new(RunStatus::Active, Some(8), 1);
    state.phases = vec!["Plan".into()];
    state.current_phase = Some("Plan".into());
    store.write_state("wf_1", &state).expect("write");
    state.revision = 3;
    store.write_state("wf_1", &state).expect("write");
    let read = store.read_state("wf_1").expect("read").expect("some");
    assert_eq!(read.revision, 3);
    assert_eq!(read.current_phase.as_deref(), Some("Plan"));
    assert_eq!(read.agent_budget, Some(8));
}

#[test]
fn unknown_manifest_version_recovers_non_terminal_runs() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "objective", SCRIPT, "{}", None, 1)
        .expect("create");
    let path = store.manifest_path();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
    value["version"] = serde_json::json!(3);
    fs::write(&path, serde_json::to_vec(&value).expect("json")).expect("write");

    let load = store.load_manifest().expect("load");
    assert!(load.recovered);
    assert_eq!(load.manifest.version, 4);
    assert_eq!(load.manifest.runs[0].status, RunStatus::Interrupted);
}

#[test]
fn corrupt_manifest_is_recovered_as_empty() {
    let (_dir, store) = store();
    fs::create_dir_all(store.session_root()).expect("mkdir");
    fs::write(store.manifest_path(), b"{not json").expect("write");
    let load = store.load_manifest().expect("load");
    assert!(load.recovered);
    assert!(load.manifest.runs.is_empty());
}

#[test]
fn manifest_update_preserves_other_runs() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "one", SCRIPT, "{}", None, 1)
        .expect("create");
    store
        .create_run("wf_2", "probe", "two", SCRIPT, "{}", None, 2)
        .expect("create");
    store
        .update_manifest_status("wf_2", RunStatus::Complete, 9)
        .expect("update");
    let manifest = store.load_manifest().expect("load").manifest;
    let one = manifest
        .runs
        .iter()
        .find(|entry| entry.run_id == "wf_1")
        .expect("one");
    let two = manifest
        .runs
        .iter()
        .find(|entry| entry.run_id == "wf_2")
        .expect("two");
    assert_eq!(one.status, RunStatus::Active);
    assert_eq!(two.status, RunStatus::Complete);
    assert_eq!(two.updated_at_ms, 9);
}

#[test]
fn scratch_round_trips_and_is_scoped_to_the_run() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "objective", SCRIPT, "{}", None, 1)
        .expect("create");
    let path = store
        .write_scratch("wf_1", "report.md", "# report\n")
        .expect("write");
    assert!(path.starts_with(store.run_layout("wf_1").expect("layout").scratch_dir()));
    assert_eq!(
        store.read_scratch("wf_1", "report.md").expect("read"),
        "# report\n"
    );
    assert!(store.read_scratch("wf_2", "report.md").is_err());
}

#[test]
fn scratch_names_reject_traversal() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "objective", SCRIPT, "{}", None, 1)
        .expect("create");
    for name in ["", "..", "../x", "a/b", "a\\b"] {
        assert!(
            store.write_scratch("wf_1", name, "x").is_err(),
            "{name:?} must be rejected"
        );
    }
}

#[test]
fn scratch_file_count_is_capped() {
    let (_dir, store) = store();
    store
        .create_run("wf_1", "probe", "objective", SCRIPT, "{}", None, 1)
        .expect("create");
    for index in 0..MAX_SCRATCH_FILES {
        store
            .write_scratch("wf_1", &format!("f{index}"), "x")
            .expect("write");
    }
    let error = store
        .write_scratch("wf_1", "overflow", "x")
        .expect_err("too many");
    assert!(matches!(error, StoreError::ScratchLimit { .. }));
}

#[test]
fn manifest_never_writes_unsupported_versions() {
    let (_dir, store) = store();
    let manifest = Manifest {
        version: 9,
        runs: Vec::new(),
    };
    store.save_manifest(&manifest).expect("save");
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(store.manifest_path()).expect("read")).expect("json");
    assert_eq!(raw["version"], serde_json::json!(4));
}
