//! Single source of truth for every workflow-related filesystem path.
//!
//! Business code must not join `.codex`, `workflows`, or a session id by hand.
//! All paths derive from [`WorkflowPaths`], which is constructed once per
//! session and cached as `Arc<WorkflowPaths>`.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowPathError {
    InvalidSessionId(String),
    InvalidRunId(String),
}

impl fmt::Display for WorkflowPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId(value) => write!(
                f,
                "invalid workflow session id {value:?}: expected a single path segment \
                 matching [A-Za-z0-9_-]"
            ),
            Self::InvalidRunId(value) => write!(
                f,
                "invalid workflow run id {value:?}: expected a single path segment \
                 matching [A-Za-z0-9_-]"
            ),
        }
    }
}

impl std::error::Error for WorkflowPathError {}

/// Resolved workflow directories for one session.
///
/// `session_root` is chosen at construction time and never recomputed. A later
/// rollout write (from `None` to `Some`) must not move an existing run root.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowPaths {
    // Kept for diagnostics and future session-scoped tooling. Deviation D-12.
    #[allow(dead_code)]
    codex_home: PathBuf,
    user_dir: PathBuf,
    bundled_dir: PathBuf,
    project_dir: Option<PathBuf>,
    project_trusted: bool,
    #[allow(dead_code)]
    session_id: String,
    session_root: PathBuf,
}

impl WorkflowPaths {
    /// Discover the git root from `cwd`, then resolve every workflow path.
    pub(crate) fn resolve(
        codex_home: PathBuf,
        cwd: &Path,
        session_id: &str,
        rollout_path: Option<&Path>,
    ) -> Result<Self, WorkflowPathError> {
        let git_root = codex_git_utils::get_git_repo_root(cwd);
        Self::resolve_with_git_root(codex_home, git_root.as_deref(), session_id, rollout_path)
    }

    /// Resolve with an explicit git root. Tests and callers that already know
    /// the repository root use this to avoid filesystem discovery.
    pub(crate) fn resolve_with_git_root(
        codex_home: PathBuf,
        git_root: Option<&Path>,
        session_id: &str,
        rollout_path: Option<&Path>,
    ) -> Result<Self, WorkflowPathError> {
        validate_segment(session_id)
            .map_err(|()| WorkflowPathError::InvalidSessionId(session_id.to_string()))?;

        let user_dir = codex_home.join("workflows");
        let bundled_dir = codex_home.join("bundled").join("workflows");
        let project_dir = git_root.map(|root| root.join(".codex").join("workflows"));
        let session_root = match rollout_path.and_then(Path::parent) {
            Some(parent) => parent.join("workflows").join(session_id),
            None => user_dir.join("sessions").join(session_id),
        };

        Ok(Self {
            codex_home,
            user_dir,
            bundled_dir,
            project_dir,
            project_trusted: false,
            session_id: session_id.to_string(),
            session_root,
        })
    }

    /// Mark whether the enclosing project is trusted for workflow discovery.
    ///
    /// Trust is resolved by the caller (folder trust lookup); `resolve` itself
    /// never reads trust state, so constructing paths stays side-effect free.
    pub(crate) fn with_project_trusted(mut self, trusted: bool) -> Self {
        self.project_trusted = trusted;
        self
    }

    /// Project scope is only scanned when a git root exists and the project is
    /// trusted. Bundled, builtin, and user scopes are independent of trust.
    pub(crate) fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    #[allow(dead_code)]
    pub(crate) fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    pub(crate) fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    pub(crate) fn bundled_dir(&self) -> &Path {
        &self.bundled_dir
    }

    pub(crate) fn project_dir(&self) -> Option<&Path> {
        self.project_dir.as_deref()
    }

    #[allow(dead_code)]
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn session_root(&self) -> &Path {
        &self.session_root
    }

    // Session-scope helpers kept for the save/resume surface. `Store` owns the
    // manifest it actually reads and writes, and `create_run` creates the run
    // directory chain, so these have no product caller yet (deviation D-17).
    #[allow(dead_code)]
    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.session_root.join("manifest.json")
    }

    /// Reserved for the bundled-asset index rebuild (deviation D-17).
    #[allow(dead_code)]
    pub(crate) fn bundled_index_path(&self) -> PathBuf {
        self.bundled_dir.join(".bundled-index.json")
    }

    /// Reserved for the bundled-asset index rebuild (deviation D-17).
    #[allow(dead_code)]
    pub(crate) fn bundled_index_lock_path(&self) -> PathBuf {
        self.bundled_dir.join(".bundled-index.lock")
    }

    /// `<session root>/<run_id>`; the run id is validated like a session id.
    pub(crate) fn run_dir(&self, run_id: &str) -> Result<PathBuf, WorkflowPathError> {
        validate_segment(run_id)
            .map_err(|()| WorkflowPathError::InvalidRunId(run_id.to_string()))?;
        Ok(self.session_root.join(run_id))
    }

    /// Create the session root and its parent chain. Callers surface the error
    /// as a launch failure instead of panicking.
    #[allow(dead_code)]
    pub(crate) fn ensure_session_root(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.session_root)
    }
}

/// A single path segment must be non-empty, at most 128 bytes, and contain only
/// `[A-Za-z0-9_-]`. `.` and `..` are rejected by the character rule.
pub(crate) fn validate_segment(value: &str) -> Result<(), ()> {
    if value.is_empty() || value.len() > 128 {
        return Err(());
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
