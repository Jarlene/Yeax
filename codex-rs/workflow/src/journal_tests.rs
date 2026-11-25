//! Tests for the workflow journal.

use super::*;

#[test]
fn record_and_replay_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::new(Some(path.clone()));
    let hash = request_hash("spawn_agent", &serde_json::json!({"prompt": "hi"}));
    journal
        .record(
            0,
            "spawn_agent",
            hash.clone(),
            serde_json::json!({"ok": true}),
        )
        .unwrap();

    let loaded = Journal::load(path).unwrap();
    assert_eq!(loaded.len(), 1);
    let replayed = loaded.replay(0, "spawn_agent", &hash).unwrap();
    assert_eq!(replayed, Some(serde_json::json!({"ok": true})));
    assert!(loaded.replay(1, "spawn_agent", &hash).unwrap().is_none());
}

#[test]
fn divergence_on_hash_mismatch() {
    let mut journal = Journal::new(None);
    journal
        .record(0, "spawn_agent", "aaaa".into(), serde_json::json!(1))
        .unwrap();
    assert!(matches!(
        journal.replay(0, "spawn_agent", "bbbb"),
        Err(JournalError::Divergence { seq: 0, .. })
    ));
    assert!(matches!(
        journal.replay(0, "budget", "aaaa"),
        Err(JournalError::Divergence { seq: 0, .. })
    ));
}

#[test]
fn torn_tail_is_truncated_before_the_next_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let first = "{\"seq\":0,\"kind\":\"log\",\"req_hash\":\"x\",\"result\":null,\"at_ms\":1}\n";
    std::fs::write(&path, format!("{first}{{\"seq\":1,\"kind")).unwrap();

    let mut journal = Journal::load(path.clone()).unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    journal
        .record(1, "log", "y".into(), serde_json::Value::Null)
        .unwrap();

    assert_eq!(Journal::load(path).unwrap().len(), 2);
}

#[test]
fn valid_unterminated_tail_is_kept_and_terminated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let line = "{\"seq\":0,\"kind\":\"log\",\"req_hash\":\"x\",\"result\":null,\"at_ms\":1}";
    std::fs::write(&path, line).unwrap();

    assert_eq!(Journal::load(path.clone()).unwrap().len(), 1);
    assert_eq!(std::fs::read_to_string(path).unwrap(), format!("{line}\n"));
}

#[cfg(unix)]
#[test]
fn load_rejects_symlink_journal() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.jsonl");
    let linked = dir.path().join("journal.jsonl");
    std::fs::write(&target, "").unwrap();
    symlink(&target, &linked).unwrap();
    assert!(matches!(
        Journal::load(linked),
        Err(JournalError::UnsafeRestore { .. })
    ));
}

#[test]
fn load_rejects_oversize_journal_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_JOURNAL_BYTES + 1).unwrap();
    assert!(matches!(
        Journal::load(path),
        Err(JournalError::UnsafeRestore { .. })
    ));
}

#[test]
fn record_refuses_to_grow_past_the_restore_cap() {
    let mut journal = Journal::new(None);
    let big = "x".repeat(MAX_JOURNAL_BYTES as usize + 1);
    let hash = request_hash("spawn_agent", &serde_json::json!({}));
    let err = journal
        .record(0, "spawn_agent", hash.clone(), serde_json::json!(big))
        .unwrap_err();
    assert!(matches!(err, JournalError::Full { seq: 0, .. }), "{err}");
    journal
        .record(0, "spawn_agent", hash, serde_json::json!({"ok": true}))
        .unwrap();
    assert_eq!(journal.len(), 1);
}

#[test]
fn complete_malformed_line_is_not_treated_as_torn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    std::fs::write(&path, b"not-json\n").unwrap();
    assert!(matches!(
        Journal::load(path),
        Err(JournalError::Parse { .. })
    ));
}

#[test]
fn load_and_record_require_dense_sequences() {
    let mut journal = Journal::new(None);
    assert!(matches!(
        journal.record(1, "log", "x".into(), serde_json::Value::Null),
        Err(JournalError::Sequence {
            expected: 0,
            actual: 1,
            ..
        })
    ));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    std::fs::write(
        &path,
        "{\"seq\":1,\"kind\":\"log\",\"req_hash\":\"x\",\"result\":null,\"at_ms\":1}\n",
    )
    .unwrap();
    assert!(matches!(
        Journal::load(path),
        Err(JournalError::Sequence {
            expected: 0,
            actual: 1,
            ..
        })
    ));
}

#[test]
fn persistence_error_does_not_advance_memory() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = Journal::new(Some(dir.path().join("journal.jsonl")));
    std::fs::create_dir(dir.path().join("journal.jsonl")).unwrap();
    assert!(matches!(
        journal.record(0, "log", "x".into(), serde_json::Value::Null),
        Err(JournalError::Io(_))
    ));
    assert!(journal.is_empty());
}

#[test]
fn prune_removes_trailing_host_error_sentinel_and_truncates_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::new(Some(path.clone()));
    journal
        .record(
            0,
            "spawn_agent",
            "aaaa".into(),
            serde_json::json!({"ok": true}),
        )
        .unwrap();
    journal
        .record(
            1,
            "write_scratch_file",
            "bbbb".into(),
            serde_json::json!({ HOST_ERROR_KEY: "scratch byte quota exceeded" }),
        )
        .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    assert_eq!(before.lines().count(), 2);

    let mut loaded = Journal::load(path.clone()).unwrap();
    assert!(
        loaded
            .prune_trailing_host_error("Runtime error: scratch byte quota exceeded")
            .unwrap()
    );
    assert_eq!(loaded.len(), 1);

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after.lines().count(), 1);
    assert!(!after.contains(HOST_ERROR_KEY));
    assert!(before.starts_with(&after), "prune must only truncate");

    loaded
        .record(
            1,
            "write_scratch_file",
            "bbbb".into(),
            serde_json::json!("ok"),
        )
        .unwrap();
    let reloaded = Journal::load(path).unwrap();
    assert_eq!(reloaded.len(), 2);
    assert_eq!(
        reloaded.replay(1, "write_scratch_file", "bbbb").unwrap(),
        Some(serde_json::json!("ok"))
    );
}

#[test]
fn prune_after_in_memory_record_truncates_and_allows_reappend() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::new(Some(path.clone()));
    journal
        .record(
            0,
            "read_scratch_file",
            "cccc".into(),
            serde_json::json!({ HOST_ERROR_KEY: "boom" }),
        )
        .unwrap();
    assert!(journal.prune_trailing_host_error("boom").unwrap());
    assert!(journal.is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    journal
        .record(
            0,
            "read_scratch_file",
            "cccc".into(),
            serde_json::json!("live"),
        )
        .unwrap();
    assert_eq!(Journal::load(path).unwrap().len(), 1);
}

#[test]
fn prune_is_a_noop_when_last_entry_is_a_success() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::new(Some(path.clone()));
    journal
        .record(
            0,
            "spawn_agent",
            "aaaa".into(),
            serde_json::json!({ HOST_ERROR_KEY: "caught mid-journal error" }),
        )
        .unwrap();
    journal
        .record(
            1,
            "spawn_agent",
            "bbbb".into(),
            serde_json::json!({"ok": true}),
        )
        .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(
        !journal
            .prune_trailing_host_error("caught mid-journal error")
            .unwrap()
    );
    assert_eq!(journal.len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn prune_is_a_noop_when_trailing_sentinel_was_caught_and_run_died_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::new(Some(path.clone()));
    journal
        .record(
            0,
            "read_scratch_file",
            "aaaa".into(),
            serde_json::json!({ HOST_ERROR_KEY: "scratch file not found: data.txt" }),
        )
        .unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(
        !journal
            .prune_trailing_host_error("Runtime error: array index out of bounds (line 9)")
            .unwrap(),
        "a caught trailing sentinel must keep replaying when the run failed elsewhere"
    );
    assert_eq!(journal.len(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn prune_is_a_noop_on_empty_journal() {
    let mut journal = Journal::new(None);
    assert!(!journal.prune_trailing_host_error("boom").unwrap());

    let dir = tempfile::tempdir().unwrap();
    let mut loaded = Journal::load(dir.path().join("missing.jsonl")).unwrap();
    assert!(!loaded.prune_trailing_host_error("boom").unwrap());
}

#[test]
fn request_hash_is_stable() {
    let a = request_hash("k", &serde_json::json!({"b": 2, "a": 1}));
    let b = request_hash("k", &serde_json::json!({"a": 1, "b": 2}));
    assert_eq!(a, b, "map key order must not affect the hash");
}
