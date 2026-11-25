//! Session manifest, run directory layout, and state persistence.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::persist;
use super::persist::PersistError;
use super::tracker::AgentRow;
use super::tracker::RunStatus;

pub(crate) const MANIFEST_VERSION: u8 = 4;
pub(crate) const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
pub(crate) const MAX_SCRIPT_BYTES: usize = 1_048_576;
pub(crate) const MAX_ARGS_BYTES: usize = 1_048_576;
pub(crate) const MAX_EFFORT_BYTES: usize = 1_024;
pub(crate) const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_SCRATCH_FILE_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_SCRATCH_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_SCRATCH_FILES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestEntry {
    pub run_id: String,
    pub display_name: String,
    pub objective: String,
    pub status: RunStatus,
    pub foreground: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub script_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u8,
    #[serde(default)]
    pub runs: Vec<ManifestEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            runs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestLoad {
    pub manifest: Manifest,
    /// Set when the on-disk manifest was missing, corrupt, or an unsupported
    /// version. Every non-terminal run is then reported as `interrupted`.
    pub recovered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunState {
    pub status: RunStatus,
    pub revision: u64,
    #[serde(default)]
    pub current_phase: Option<String>,
    #[serde(default)]
    pub phases: Vec<String>,
    pub agents_used: u64,
    #[serde(default)]
    pub agent_budget: Option<u64>,
    #[serde(default)]
    pub result_summary: Option<String>,
    #[serde(default)]
    pub agents: Vec<AgentRow>,
    pub updated_at_ms: u64,
}

impl RunState {
    #[cfg(test)]
    pub(crate) fn new(status: RunStatus, agent_budget: Option<u64>, updated_at_ms: u64) -> Self {
        Self {
            status,
            revision: 0,
            current_phase: None,
            phases: Vec::new(),
            agents_used: 0,
            agent_budget,
            result_summary: None,
            agents: Vec::new(),
            updated_at_ms,
        }
    }
}

#[derive(Debug)]
pub(crate) enum StoreError {
    Persist(PersistError),
    InvalidState { path: PathBuf, reason: String },
    ScratchLimit { reason: &'static str },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persist(error) => write!(f, "{error}"),
            Self::InvalidState { path, reason } => {
                write!(f, "{}: invalid workflow state: {reason}", path.display())
            }
            Self::ScratchLimit { reason } => write!(f, "workflow scratch limit: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<PersistError> for StoreError {
    fn from(error: PersistError) -> Self {
        Self::Persist(error)
    }
}

/// Paths for one run directory.
#[derive(Debug, Clone)]
pub(crate) struct RunLayout {
    root: PathBuf,
}

impl RunLayout {
    // Reserved for callers that already resolved a run root. Deviation D-13.
    #[allow(dead_code)]
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn script_path(&self) -> PathBuf {
        self.root.join("script.rhai")
    }

    pub(crate) fn args_path(&self) -> PathBuf {
        self.root.join("args.json")
    }

    pub(crate) fn effort_path(&self) -> PathBuf {
        self.root.join("effort")
    }

    pub(crate) fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    #[allow(dead_code)]
    pub(crate) fn journal_path(&self) -> PathBuf {
        self.root.join("journal.jsonl")
    }

    pub(crate) fn scratch_dir(&self) -> PathBuf {
        self.root.join("scratch")
    }

    pub(crate) fn scratch_path(&self, name: &str) -> Result<PathBuf, StoreError> {
        persist::validate_file_name(name)?;
        Ok(self.scratch_dir().join(name))
    }
}

/// Session-level workflow store.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    session_root: PathBuf,
}

impl Store {
    pub(crate) fn new(session_root: PathBuf) -> Self {
        Self { session_root }
    }

    #[allow(dead_code)]
    pub(crate) fn session_root(&self) -> &Path {
        &self.session_root
    }

    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.session_root.join("manifest.json")
    }

    pub(crate) fn run_layout(&self, run_id: &str) -> Result<RunLayout, StoreError> {
        persist::validate_file_name(run_id)?;
        Ok(RunLayout::new(self.session_root.join(run_id)))
    }

    /// Load v4. A corrupt file or unknown version is reported as `recovered`
    /// with every non-terminal run marked `interrupted`.
    pub(crate) fn load_manifest(&self) -> Result<ManifestLoad, StoreError> {
        let path = self.manifest_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ManifestLoad {
                    manifest: Manifest::default(),
                    recovered: false,
                });
            }
            Err(source) => {
                return Err(StoreError::Persist(PersistError::Io { path, source }));
            }
        };
        match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(mut manifest) if manifest.version == MANIFEST_VERSION => {
                manifest.runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
                Ok(ManifestLoad {
                    manifest,
                    recovered: false,
                })
            }
            Ok(mut manifest) => {
                recover(&mut manifest);
                recovered_load(manifest)
            }
            Err(_) => recovered_load(Manifest::default()),
        }
    }

    pub(crate) fn save_manifest(&self, manifest: &Manifest) -> Result<(), StoreError> {
        let mut manifest = manifest.clone();
        manifest.version = MANIFEST_VERSION;
        manifest.runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        let bytes =
            serde_json::to_vec_pretty(&manifest).map_err(|error| StoreError::InvalidState {
                path: self.manifest_path(),
                reason: error.to_string(),
            })?;
        let max = usize::try_from(MAX_MANIFEST_BYTES).unwrap_or(usize::MAX);
        persist::atomic_write(&self.manifest_path(), &bytes, max)?;
        Ok(())
    }

    /// Freeze script, args, and effort with create-only semantics.
    // The run layout needs every frozen field; grouping them into a struct
    // would duplicate the caller-side launch records.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_run(
        &self,
        run_id: &str,
        display_base: &str,
        objective: &str,
        script: &str,
        args_json: &str,
        effort: Option<&str>,
        created_at_ms: u64,
    ) -> Result<RunLayout, StoreError> {
        let layout = self.run_layout(run_id)?;
        std::fs::create_dir_all(layout.root()).map_err(|source| {
            StoreError::Persist(PersistError::Io {
                path: layout.root().to_path_buf(),
                source,
            })
        })?;
        persist::create_new(&layout.script_path(), script.as_bytes(), MAX_SCRIPT_BYTES)?;
        persist::create_new(&layout.args_path(), args_json.as_bytes(), MAX_ARGS_BYTES)?;
        persist::create_new(
            &layout.effort_path(),
            effort.unwrap_or_default().as_bytes(),
            MAX_EFFORT_BYTES,
        )?;
        std::fs::create_dir_all(layout.scratch_dir()).map_err(|source| {
            StoreError::Persist(PersistError::Io {
                path: layout.scratch_dir(),
                source,
            })
        })?;

        let mut manifest = self.load_manifest()?.manifest;
        let display_name = unique_display_name(&manifest, display_base);
        manifest.runs.push(ManifestEntry {
            run_id: run_id.to_string(),
            display_name,
            objective: objective.to_string(),
            status: RunStatus::Active,
            foreground: true,
            created_at_ms,
            updated_at_ms: created_at_ms,
            script_path: Some(layout.script_path().display().to_string()),
        });
        self.save_manifest(&manifest)?;
        Ok(layout)
    }

    pub(crate) fn write_state(&self, run_id: &str, state: &RunState) -> Result<(), StoreError> {
        let layout = self.run_layout(run_id)?;
        let bytes = serde_json::to_vec_pretty(state).map_err(|error| StoreError::InvalidState {
            path: layout.state_path(),
            reason: error.to_string(),
        })?;
        let max = usize::try_from(MAX_STATE_BYTES).unwrap_or(usize::MAX);
        persist::atomic_write(&layout.state_path(), &bytes, max)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn read_state(&self, run_id: &str) -> Result<Option<RunState>, StoreError> {
        let layout = self.run_layout(run_id)?;
        let bytes = match std::fs::read(layout.state_path()) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(StoreError::Persist(PersistError::Io {
                    path: layout.state_path(),
                    source,
                }));
            }
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| StoreError::InvalidState {
                path: layout.state_path(),
                reason: error.to_string(),
            })
    }

    pub(crate) fn update_manifest_status(
        &self,
        run_id: &str,
        status: RunStatus,
        updated_at_ms: u64,
    ) -> Result<(), StoreError> {
        let mut manifest = self.load_manifest()?.manifest;
        if let Some(entry) = manifest
            .runs
            .iter_mut()
            .find(|entry| entry.run_id == run_id)
        {
            entry.status = status;
            entry.updated_at_ms = updated_at_ms;
        }
        self.save_manifest(&manifest)
    }

    pub(crate) fn write_scratch(
        &self,
        run_id: &str,
        name: &str,
        content: &str,
    ) -> Result<PathBuf, StoreError> {
        let layout = self.run_layout(run_id)?;
        let path = layout.scratch_path(name)?;
        std::fs::create_dir_all(layout.scratch_dir()).map_err(|source| {
            StoreError::Persist(PersistError::Io {
                path: layout.scratch_dir(),
                source,
            })
        })?;
        persist::ensure_within(&layout.scratch_dir(), &path)?;
        let (files, total) = scratch_usage(&layout.scratch_dir())?;
        let existing = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let incoming = u64::try_from(content.len()).unwrap_or(u64::MAX);
        if existing == 0 && files >= MAX_SCRATCH_FILES as u64 {
            return Err(StoreError::ScratchLimit {
                reason: "too many scratch files",
            });
        }
        if total.saturating_sub(existing).saturating_add(incoming) > MAX_SCRATCH_TOTAL_BYTES {
            return Err(StoreError::ScratchLimit {
                reason: "scratch total size exceeded",
            });
        }
        persist::atomic_write(&path, content.as_bytes(), MAX_SCRATCH_FILE_BYTES)?;
        Ok(path)
    }

    pub(crate) fn read_scratch(&self, run_id: &str, name: &str) -> Result<String, StoreError> {
        let layout = self.run_layout(run_id)?;
        let path = layout.scratch_path(name)?;
        persist::ensure_within(&layout.scratch_dir(), &path)?;
        persist::reject_symlink(&path)?;
        std::fs::read_to_string(&path)
            .map_err(|source| StoreError::Persist(PersistError::Io { path, source }))
    }
}

fn recover(manifest: &mut Manifest) -> bool {
    manifest.version = MANIFEST_VERSION;
    for entry in &mut manifest.runs {
        if !entry.status.is_terminal() {
            entry.status = RunStatus::Interrupted;
        }
    }
    true
}

fn recovered_load(mut manifest: Manifest) -> Result<ManifestLoad, StoreError> {
    let _ = recover(&mut manifest);
    Ok(ManifestLoad {
        manifest,
        recovered: true,
    })
}

fn unique_display_name(manifest: &Manifest, base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "workflow" } else { base };
    let base = base.chars().take(48).collect::<String>();
    let used: BTreeSet<&str> = manifest
        .runs
        .iter()
        .map(|entry| entry.display_name.as_str())
        .collect();
    if !used.contains(base.as_str()) {
        return base;
    }
    for suffix in 2u32.. {
        let candidate = format!("{base}-{suffix}");
        if !used.contains(candidate.as_str()) {
            return candidate;
        }
    }
    base
}

fn scratch_usage(dir: &Path) -> Result<(u64, u64), StoreError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(source) => {
            return Err(StoreError::Persist(PersistError::Io {
                path: dir.to_path_buf(),
                source,
            }));
        }
    };
    let mut files = 0u64;
    let mut total = 0u64;
    for entry in entries {
        let entry = entry.map_err(|source| {
            StoreError::Persist(PersistError::Io {
                path: dir.to_path_buf(),
                source,
            })
        })?;
        let meta = entry.metadata().map_err(|source| {
            StoreError::Persist(PersistError::Io {
                path: entry.path(),
                source,
            })
        })?;
        if meta.is_file() {
            files += 1;
            total = total.saturating_add(meta.len());
        }
    }
    Ok((files, total))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
