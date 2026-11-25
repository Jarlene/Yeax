//! Tests for workflow persistence primitives.

use std::fs;

use tempfile::TempDir;

use super::*;

#[test]
fn create_new_writes_once_and_rejects_overwrite() {
    let dir = TempDir::new().expect("temp");
    let path = dir.path().join("run").join("script.rhai");
    create_new(&path, b"first", 1024).expect("create");
    let error = create_new(&path, b"second", 1024).expect_err("exists");
    assert!(error.is_exists());
    assert_eq!(fs::read_to_string(&path).expect("read"), "first");
    assert!(!leftover_temp(dir.path()));
}

#[test]
fn create_new_rejects_oversized_content() {
    let dir = TempDir::new().expect("temp");
    let path = dir.path().join("script.rhai");
    let error = create_new(&path, &[b'x'; 10], 4).expect_err("too large");
    assert!(matches!(error, PersistError::TooLarge { .. }));
    assert!(!path.exists());
}

#[test]
fn atomic_write_replaces_and_leaves_no_temp() {
    let dir = TempDir::new().expect("temp");
    let path = dir.path().join("state.json");
    atomic_write(&path, b"{\"a\":1}", 1024).expect("first");
    atomic_write(&path, b"{\"a\":2}", 1024).expect("second");
    assert_eq!(fs::read_to_string(&path).expect("read"), "{\"a\":2}");
    assert!(!leftover_temp(dir.path()));
}

#[test]
fn append_line_never_rewrites_existing_content() {
    let dir = TempDir::new().expect("temp");
    let path = dir.path().join("journal.jsonl");
    append_line(&path, "one", 1024, 10).expect("one");
    append_line(&path, "two", 1024, 10).expect("two");
    assert_eq!(fs::read_to_string(&path).expect("read"), "one\ntwo\n");
}

#[test]
fn append_line_enforces_size_and_count_limits() {
    let dir = TempDir::new().expect("temp");
    let path = dir.path().join("journal.jsonl");
    append_line(&path, "1234", 6, 10).expect("fits");
    let error = append_line(&path, "1234", 6, 10).expect_err("size");
    assert!(matches!(error, PersistError::Limit { .. }));
    assert_eq!(fs::read_to_string(&path).expect("read"), "1234\n");

    let path = dir.path().join("journal2.jsonl");
    append_line(&path, "a", 1024, 1).expect("one");
    let error = append_line(&path, "b", 1024, 1).expect_err("count");
    assert!(matches!(error, PersistError::Limit { .. }));
}

#[test]
fn trailing_partial_record_is_ignored() {
    let dir = TempDir::new().expect("temp");
    let path = dir.path().join("journal.jsonl");
    fs::write(&path, "{\"seq\":1}\n{\"seq\":2}\n{\"seq\":3").expect("write");
    assert_eq!(
        read_lines_ignore_trailing_partial(&path).expect("read"),
        vec!["{\"seq\":1}", "{\"seq\":2}"]
    );
}

#[test]
fn missing_append_target_reads_as_empty() {
    let dir = TempDir::new().expect("temp");
    assert!(
        read_lines_ignore_trailing_partial(&dir.path().join("none.jsonl"))
            .expect("read")
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_rejected() {
    let dir = TempDir::new().expect("temp");
    let real = dir.path().join("real");
    fs::create_dir_all(&real).expect("mkdir");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let error = create_new(&link.join("file"), b"x", 16).expect_err("symlink");
    assert!(matches!(error, PersistError::Symlink(_)));
}

#[test]
fn ensure_within_accepts_children_and_rejects_escapes() {
    let dir = TempDir::new().expect("temp");
    let root = dir.path().join("run");
    fs::create_dir_all(&root).expect("mkdir");
    fs::create_dir_all(dir.path().join("other")).expect("mkdir");
    ensure_within(&root, &root.join("scratch").join("report.md")).expect("child");
    let error = ensure_within(&root, &dir.path().join("other").join("x")).expect_err("outside");
    assert!(matches!(error, PersistError::OutsideRoot { .. }));
}

#[test]
fn file_names_must_be_single_safe_segments() {
    for name in ["report.md", "a-b_c.1"] {
        validate_file_name(name).expect("valid");
    }
    for name in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
        assert!(
            validate_file_name(name).is_err(),
            "{name:?} must be rejected"
        );
    }
}

fn leftover_temp(root: &Path) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".tmp"))
            {
                return true;
            }
        }
    }
    false
}
