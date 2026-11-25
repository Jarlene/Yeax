//! Append-only workflow journal with size limits and content hashing.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use sha2::Digest as _;

pub const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_JOURNAL_ENTRIES: usize = crate::MAX_HOST_CALLS as usize;

pub(crate) const HOST_ERROR_KEY: &str = "__codex_workflow_host_error";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    pub seq: u64,
    pub kind: String,
    pub req_hash: String,
    pub result: serde_json::Value,
    pub at_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal io: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal parse at line {line}: {error}")]
    Parse { line: usize, error: String },
    #[error("journal restore rejected (limit {limit}): {reason}")]
    UnsafeRestore { limit: u64, reason: String },
    #[error(
        "journal full: appending seq {seq} would exceed the {limit}-byte cap \
         that restore enforces, which would strand the run unresumable"
    )]
    Full { seq: u64, limit: u64 },
    #[error("journal is not dense at entry {index}: expected sequence {expected}, found {actual}")]
    Sequence {
        index: usize,
        expected: u64,
        actual: u64,
    },
    #[error(
        "replay divergence at seq {seq} ({kind}): the script issued a different call than the \
         recorded run — the workflow script is nondeterministic or was edited mid-run"
    )]
    Divergence { seq: u64, kind: String },
}

#[derive(Debug, Default)]
pub struct Journal {
    entries: Vec<JournalEntry>,
    path: Option<PathBuf>,
    bytes: u64,
    last_line_start: Option<u64>,
}

impl Journal {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            entries: Vec::new(),
            path,
            bytes: 0,
            last_line_start: None,
        }
    }

    pub fn load(path: PathBuf) -> Result<Self, JournalError> {
        let content = match read_journal_bounded(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(JournalError::UnsafeRestore {
                    limit: MAX_JOURNAL_BYTES,
                    reason: error.to_string(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut entries = Vec::new();
        let mut offset = 0usize;
        let mut line_number = 0usize;
        let mut bytes = content.len() as u64;
        let mut last_line_start = None;
        while offset < content.len() {
            line_number += 1;
            let Some(rest) = content.get(offset..) else {
                break;
            };
            let Some(relative_newline) = rest.iter().position(|byte| *byte == b'\n') else {
                let tail = rest;
                if tail.iter().all(u8::is_ascii_whitespace) {
                    truncate_tail(&path, offset as u64)?;
                    bytes = offset as u64;
                    break;
                }
                match serde_json::from_slice::<JournalEntry>(tail) {
                    Ok(entry) => {
                        if entries.len() >= MAX_JOURNAL_ENTRIES {
                            return Err(JournalError::UnsafeRestore {
                                limit: MAX_JOURNAL_ENTRIES as u64,
                                reason: "too many journal entries".into(),
                            });
                        }
                        validate_sequence(&entries, &entry)?;
                        entries.push(entry);
                        last_line_start = Some(offset as u64);
                        terminate_line(&path)?;
                        bytes = bytes.saturating_add(1);
                    }
                    Err(error) => {
                        tracing::warn!(
                            line = line_number,
                            %error,
                            "truncating torn workflow journal tail"
                        );
                        truncate_tail(&path, offset as u64)?;
                        bytes = offset as u64;
                    }
                }
                break;
            };
            let end = offset + relative_newline;
            let Some(line) = content.get(offset..end) else {
                break;
            };
            let line_start = offset as u64;
            offset = end + 1;
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let entry = serde_json::from_slice::<JournalEntry>(line).map_err(|error| {
                JournalError::Parse {
                    line: line_number,
                    error: error.to_string(),
                }
            })?;
            if entries.len() >= MAX_JOURNAL_ENTRIES {
                return Err(JournalError::UnsafeRestore {
                    limit: MAX_JOURNAL_ENTRIES as u64,
                    reason: "too many journal entries".into(),
                });
            }
            validate_sequence(&entries, &entry)?;
            entries.push(entry);
            last_line_start = Some(line_start);
        }
        Ok(Self {
            entries,
            path: Some(path),
            bytes,
            last_line_start,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn agent_reservation_count(&self) -> u64 {
        u64::try_from(
            self.entries
                .iter()
                .filter(|entry| entry.kind == "spawn_agent")
                .count(),
        )
        .unwrap_or(u64::MAX)
    }

    pub fn covers(&self, seq: u64) -> bool {
        usize::try_from(seq).is_ok_and(|seq| seq < self.entries.len())
    }

    pub fn replay(
        &self,
        seq: u64,
        kind: &str,
        req_hash: &str,
    ) -> Result<Option<serde_json::Value>, JournalError> {
        let Some(entry) = usize::try_from(seq)
            .ok()
            .and_then(|seq| self.entries.get(seq))
        else {
            return Ok(None);
        };
        if entry.seq != seq || entry.kind != kind || entry.req_hash != req_hash {
            return Err(JournalError::Divergence {
                seq,
                kind: kind.to_string(),
            });
        }
        Ok(Some(entry.result.clone()))
    }

    pub fn record(
        &mut self,
        seq: u64,
        kind: &str,
        req_hash: String,
        result: serde_json::Value,
    ) -> Result<(), JournalError> {
        let entry = JournalEntry {
            seq,
            kind: kind.to_string(),
            req_hash,
            result,
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        validate_sequence(&self.entries, &entry)?;
        let mut line = serde_json::to_string(&entry)
            .map_err(|error| JournalError::Io(std::io::Error::other(error)))?;
        line.push('\n');
        if self.bytes.saturating_add(line.len() as u64) > MAX_JOURNAL_BYTES {
            return Err(JournalError::Full {
                seq,
                limit: MAX_JOURNAL_BYTES,
            });
        }
        if let Some(path) = &self.path {
            append_line(path, &line)?;
        }
        self.last_line_start = Some(self.bytes);
        self.bytes = self.bytes.saturating_add(line.len() as u64);
        self.entries.push(entry);
        Ok(())
    }

    pub fn prune_trailing_host_error(
        &mut self,
        failure_detail: &str,
    ) -> Result<bool, JournalError> {
        let Some(last) = self.entries.last() else {
            return Ok(false);
        };
        let Some(message) = last.result.get(HOST_ERROR_KEY).and_then(|v| v.as_str()) else {
            return Ok(false);
        };
        if message.is_empty() || !failure_detail.contains(message) {
            return Ok(false);
        }
        let Some(new_len) = self.last_line_start else {
            return Err(JournalError::Io(std::io::Error::other(
                "journal cannot locate the trailing entry's byte offset",
            )));
        };
        if let Some(path) = &self.path {
            truncate_tail(path, new_len)?;
        }
        self.entries.pop();
        self.bytes = new_len;
        self.last_line_start = None;
        Ok(true)
    }
}

fn read_journal_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal is not a regular file: {}", path.display()),
        ));
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() > MAX_JOURNAL_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "journal changed during open",
        ));
    }
    let mut content = Vec::with_capacity(opened.len() as usize);
    file.take(MAX_JOURNAL_BYTES.saturating_add(1))
        .read_to_end(&mut content)?;
    if content.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("journal exceeds {MAX_JOURNAL_BYTES} bytes"),
        ));
    }
    Ok(content)
}

fn validate_sequence(entries: &[JournalEntry], entry: &JournalEntry) -> Result<(), JournalError> {
    let expected = entries.len() as u64;
    if entry.seq != expected {
        return Err(JournalError::Sequence {
            index: entries.len(),
            expected,
            actual: entry.seq,
        });
    }
    Ok(())
}

fn truncate_tail(path: &Path, len: u64) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    file.sync_data()
}

fn terminate_line(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.sync_data()
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), canonical_json(v)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_json).collect())
        }
        other => other.clone(),
    }
}

pub fn request_hash(kind: &str, payload: &serde_json::Value) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical_json(payload).to_string().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
