//! The three persistence strategies used by workflow storage.
//!
//! | Strategy | Files | Overwrite |
//! | --- | --- | --- |
//! | create-only | `script.rhai`, `args.json`, `effort`, project saves, first bundled install | no |
//! | atomic replace | `manifest.json`, `state.json`, `scratch/*`, bundled upgrades | yes |
//! | append | `journal.jsonl` | never |
//!
//! Every strategy writes a complete temporary file first. Create-only never
//! deletes or truncates an existing target; a no-replace publish reports
//! [`PersistError::Exists`] and leaves the target untouched.

use std::fmt;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum PersistError {
    Exists(PathBuf),
    TooLarge {
        path: PathBuf,
        max: usize,
        actual: usize,
    },
    // Reserved for bounded append targets (journal). Deviation D-13.
    #[allow(dead_code)]
    Limit {
        path: PathBuf,
        reason: &'static str,
    },
    Symlink(PathBuf),
    OutsideRoot {
        root: PathBuf,
        target: PathBuf,
    },
    InvalidName(String),
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exists(path) => write!(f, "{}: already exists", path.display()),
            Self::TooLarge { path, max, actual } => write!(
                f,
                "{}: content is {actual} bytes, maximum is {max}",
                path.display()
            ),
            Self::Limit { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::Symlink(path) => write!(
                f,
                "{}: symbolic links and reparse points are not allowed",
                path.display()
            ),
            Self::OutsideRoot { root, target } => write!(
                f,
                "{}: resolves outside the allowed root {}",
                target.display(),
                root.display()
            ),
            Self::InvalidName(name) => write!(
                f,
                "invalid file name {name:?}: expected a single safe path segment"
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for PersistError {}

impl PersistError {
    pub(crate) fn is_exists(&self) -> bool {
        matches!(self, Self::Exists(_))
    }
}

fn io_error(path: &Path, source: io::Error) -> PersistError {
    PersistError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Create `path` with `bytes`, failing if it already exists.
pub(crate) fn create_new(path: &Path, bytes: &[u8], max: usize) -> Result<(), PersistError> {
    if bytes.len() > max {
        return Err(PersistError::TooLarge {
            path: path.to_path_buf(),
            max,
            actual: bytes.len(),
        });
    }
    let parent = ensure_parent(path)?;
    reject_symlink(path)?;
    let temp = temp_path(&parent, path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|source| io_error(&temp, source))?;
    if let Err(source) = write_all_sync(&mut file, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(&temp, source));
    }
    drop(file);
    let published = publish_no_replace(&temp, path);
    if published.is_err() {
        let _ = fs::remove_file(&temp);
    }
    published
}

/// Replace `path` atomically; readers see either the old or the new file.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], max: usize) -> Result<(), PersistError> {
    if bytes.len() > max {
        return Err(PersistError::TooLarge {
            path: path.to_path_buf(),
            max,
            actual: bytes.len(),
        });
    }
    let parent = ensure_parent(path)?;
    reject_symlink(path)?;
    let temp = temp_path(&parent, path)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|source| io_error(&temp, source))?;
    if let Err(source) = write_all_sync(&mut file, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(&temp, source));
    }
    drop(file);
    if let Err(source) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(io_error(path, source));
    }
    Ok(())
}

/// Append one complete line. Existing content is never truncated or rewritten.
// Journal append/read helpers are reserved for the run journal path. D-13.
#[allow(dead_code)]
pub(crate) fn append_line(
    path: &Path,
    line: &str,
    max_total_bytes: u64,
    max_lines: u64,
) -> Result<(), PersistError> {
    let parent = ensure_parent(path)?;
    let _ = parent;
    reject_symlink(path)?;
    let existing_len = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let incoming = u64::try_from(line.len()).unwrap_or(u64::MAX) + 1;
    if existing_len.saturating_add(incoming) > max_total_bytes {
        return Err(PersistError::Limit {
            path: path.to_path_buf(),
            reason: "append would exceed the maximum file size",
        });
    }
    if count_lines(path)? >= max_lines {
        return Err(PersistError::Limit {
            path: path.to_path_buf(),
            reason: "append would exceed the maximum line count",
        });
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let mut buffer = String::with_capacity(line.len() + 1);
    buffer.push_str(line);
    buffer.push('\n');
    file.write_all(buffer.as_bytes())
        .map_err(|source| io_error(path, source))?;
    file.sync_data().map_err(|source| io_error(path, source))?;
    Ok(())
}

/// Read newline-delimited records, dropping a trailing partial record left by
/// a crash. Interior parse failures are the caller's responsibility.
#[allow(dead_code)]
pub(crate) fn read_lines_ignore_trailing_partial(path: &Path) -> Result<Vec<String>, PersistError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path, source)),
    };
    let text = String::from_utf8(bytes).map_err(|_| PersistError::Limit {
        path: path.to_path_buf(),
        reason: "file is not valid UTF-8",
    })?;
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    // `split` yields a trailing empty element when the file ends with '\n'.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    } else {
        // No trailing newline: the last record is a partial write.
        lines.pop();
    }
    Ok(lines.into_iter().filter(|line| !line.is_empty()).collect())
}

#[allow(dead_code)]
fn count_lines(path: &Path) -> Result<u64, PersistError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(io_error(path, source)),
    };
    Ok(u64::try_from(bytes.iter().filter(|byte| **byte == b'\n').count()).unwrap_or(u64::MAX))
}

fn ensure_parent(path: &Path) -> Result<PathBuf, PersistError> {
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| PersistError::InvalidName(path.display().to_string()))?;
    fs::create_dir_all(&parent).map_err(|source| io_error(&parent, source))?;
    Ok(parent)
}

fn temp_path(parent: &Path, target: &Path) -> Result<PathBuf, PersistError> {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PersistError::InvalidName(target.display().to_string()))?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".{name}.{}.{counter}.tmp", std::process::id())))
}

fn write_all_sync(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

/// Publish `temp` at `target` without replacing an existing target.
///
/// `hard_link` fails with `EEXIST` when the target exists on every supported
/// platform, which gives the same no-replace guarantee as `renameat2` and
/// `MoveFileExW` without platform-specific unsafe code. The temporary file is
/// removed afterwards; a crash between link and unlink leaves one complete
/// hard link that startup cleanup removes by prefix.
fn publish_no_replace(temp: &Path, target: &Path) -> Result<(), PersistError> {
    match fs::hard_link(temp, target) {
        Ok(()) => {
            let _ = fs::remove_file(temp);
            Ok(())
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            Err(PersistError::Exists(target.to_path_buf()))
        }
        Err(source) => Err(io_error(target, source)),
    }
}

/// Reject a target whose final component or any existing ancestor is a symlink
/// or Windows reparse point.
pub(crate) fn reject_symlink(path: &Path) -> Result<(), PersistError> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        match fs::symlink_metadata(candidate) {
            Ok(meta) if is_link_like(&meta) => {
                return Err(PersistError::Symlink(candidate.to_path_buf()));
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(candidate, source)),
        }
        current = candidate.parent();
    }
    Ok(())
}

fn is_link_like(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Verify `target` resolves inside `root`. When the target does not exist yet,
/// its nearest existing parent is canonicalized instead.
pub(crate) fn ensure_within(root: &Path, target: &Path) -> Result<(), PersistError> {
    let root = fs::canonicalize(root).map_err(|source| io_error(root, source))?;
    let mut probe = target.to_path_buf();
    loop {
        match fs::canonicalize(&probe) {
            Ok(resolved) => {
                if resolved.starts_with(&root) {
                    return Ok(());
                }
                return Err(PersistError::OutsideRoot {
                    root,
                    target: target.to_path_buf(),
                });
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                if !probe.pop() {
                    return Err(PersistError::OutsideRoot {
                        root,
                        target: target.to_path_buf(),
                    });
                }
            }
            Err(source) => return Err(io_error(&probe, source)),
        }
    }
}

/// Scratch and argument file names must be one safe segment.
pub(crate) fn validate_file_name(name: &str) -> Result<(), PersistError> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0');
    if valid {
        Ok(())
    } else {
        Err(PersistError::InvalidName(name.to_string()))
    }
}

#[cfg(test)]
#[path = "persist_tests.rs"]
mod tests;
