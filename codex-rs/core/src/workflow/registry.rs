//! Four-scope workflow registry: `bundled > builtin > project > user`.
//!
//! Construction only discovers candidates; it never writes to disk. Discovery
//! is depth-1, accepts only plain `*.rhai` files whose stem matches
//! `meta.name`, and rejects symlinks / Windows reparse points.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use codex_workflow::{MetaError, WorkflowMeta, extract_meta};

use super::assets;
use super::paths::WorkflowPaths;
use super::persist;

pub(crate) const MAX_WORKFLOW_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_WORKFLOW_NAME_BYTES: usize = 64;

/// One injected candidate: parsed meta, source body, and its origin path.
#[cfg(test)]
type CandidateEntry = (WorkflowMeta, String, PathBuf);

/// Injected candidates grouped per scope, in priority order.
#[cfg(test)]
type ScopedCandidates = Vec<(WorkflowScope, Vec<CandidateEntry>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum WorkflowScope {
    Bundled,
    Builtin,
    Project,
    User,
}

impl WorkflowScope {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Builtin => "builtin",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowOrigin {
    Builtin,
    Inline,
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedWorkflow {
    pub meta: WorkflowMeta,
    pub script: String,
    pub origin: WorkflowOrigin,
    /// `None` for inline scripts; otherwise the scope that won resolution.
    pub scope: Option<WorkflowScope>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct WorkflowListing {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    pub scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Set when a higher-priority scope shadows this candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) enum RegistryError {
    InvalidName(String),
    UnknownName(String),
    DuplicateName {
        name: String,
        scope: WorkflowScope,
        sources: Vec<String>,
    },
    UntrustedPath {
        path: String,
        reason: String,
    },
    SymlinkRejected {
        path: String,
    },
    SourceTooLarge {
        path: String,
        limit: usize,
    },
    InvalidFilename(String),
    FilenameMismatch {
        filename: String,
        name: String,
    },
    SaveExists {
        path: PathBuf,
    },
    Io {
        path: String,
        error: String,
    },
    Meta(MetaError),
}

impl RegistryError {
    /// Stable tool-facing error code. Save-exists and symlink rejections keep
    /// their dedicated codes; every other discovery/parse failure collapses to
    /// `workflow_resolve_failed` per design 03 §5.5.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::SaveExists { .. } => "workflow_save_exists",
            Self::SymlinkRejected { .. } => "workflow_script_symlink_rejected",
            _ => "workflow_resolve_failed",
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "invalid workflow name {name:?}: expected 1-64 lowercase letters, digits, \
                 or single hyphens"
            ),
            Self::UnknownName(name) => write!(f, "unknown workflow: {name}"),
            Self::DuplicateName {
                name,
                scope,
                sources,
            } => write!(
                f,
                "ambiguous workflow {name:?}: duplicate definitions in {} scope ({})",
                scope.label(),
                sources.join(", ")
            ),
            Self::UntrustedPath { path, reason } => {
                write!(f, "workflow path is not trusted: {path} ({reason})")
            }
            Self::SymlinkRejected { path } => {
                write!(f, "workflow script is a symlink or reparse point: {path}")
            }
            Self::SourceTooLarge { path, limit } => {
                write!(f, "workflow source exceeds {limit} bytes: {path}")
            }
            Self::InvalidFilename(name) => write!(
                f,
                "invalid workflow filename {name:?}: expected <safe-name>.rhai"
            ),
            Self::FilenameMismatch { filename, name } => write!(
                f,
                "saved workflow filename {filename:?} must match meta.name {name:?}"
            ),
            Self::SaveExists { path } => {
                write!(f, "workflow already exists: {}", path.display())
            }
            Self::Io { path, error } => write!(f, "failed to access {path}: {error}"),
            Self::Meta(error) => write!(f, "invalid workflow script: {error}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<MetaError> for RegistryError {
    fn from(error: MetaError) -> Self {
        Self::Meta(error)
    }
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    meta: WorkflowMeta,
    script: String,
    origin: WorkflowOrigin,
    scope: WorkflowScope,
    path: Option<PathBuf>,
    source_label: String,
}

pub(crate) struct WorkflowRegistry {
    /// Winning entry per name, in priority order.
    entries: Vec<RegistryEntry>,
    /// Lower-priority candidates shadowed by a winning entry.
    shadowed: Vec<RegistryEntry>,
    duplicates: BTreeMap<String, (WorkflowScope, Vec<String>)>,
}

fn builtin_entries() -> &'static Vec<RegistryEntry> {
    static CACHE: OnceLock<Vec<RegistryEntry>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut entries = Vec::new();
        for asset in assets::builtin_scripts() {
            let script = asset.content;
            if script.len() > MAX_WORKFLOW_SOURCE_BYTES {
                continue;
            }
            let Ok(meta) = extract_meta(script) else {
                continue;
            };
            if meta.name != asset.name || validate_name(&meta.name).is_err() {
                continue;
            }
            entries.push(RegistryEntry {
                meta,
                script: script.to_string(),
                origin: WorkflowOrigin::Builtin,
                scope: WorkflowScope::Builtin,
                path: Some(PathBuf::from(asset.path)),
                source_label: "builtin".to_string(),
            });
        }
        entries
    })
}

impl WorkflowRegistry {
    /// Sole constructor. It performs directory discovery only; no file is
    /// created, moved, or rewritten.
    pub(crate) fn new(paths: Arc<WorkflowPaths>) -> Result<Self, RegistryError> {
        let mut entries = Vec::new();
        let mut shadowed = Vec::new();
        let mut duplicates = BTreeMap::new();

        let scoped: Vec<(WorkflowScope, Vec<RegistryEntry>)> = vec![
            (
                WorkflowScope::Bundled,
                scan_directory(paths.bundled_dir(), WorkflowScope::Bundled, true),
            ),
            (WorkflowScope::Builtin, builtin_entries().to_vec()),
        ];
        merge_scopes(&mut entries, &mut shadowed, &mut duplicates, scoped);

        if paths.project_trusted()
            && let Some(project_dir) = paths.project_dir()
        {
            merge_scopes(
                &mut entries,
                &mut shadowed,
                &mut duplicates,
                vec![(
                    WorkflowScope::Project,
                    scan_directory(project_dir, WorkflowScope::Project, false),
                )],
            );
        }

        merge_scopes(
            &mut entries,
            &mut shadowed,
            &mut duplicates,
            vec![(
                WorkflowScope::User,
                scan_directory(paths.user_dir(), WorkflowScope::User, false),
            )],
        );

        entries.sort_by(|left, right| left.meta.name.cmp(&right.meta.name));
        shadowed.sort_by(|left, right| left.meta.name.cmp(&right.meta.name));
        Ok(Self {
            entries,
            shadowed,
            duplicates,
        })
    }

    /// Test/P1 candidate injection (design 03 §4.2 rule 9). Candidates are
    /// supplied in priority order and used to validate shadowing without a
    /// filesystem or trust lookup.
    #[cfg(test)]
    pub(crate) fn from_candidates(scopes: ScopedCandidates) -> Self {
        let mut entries = Vec::new();
        let mut shadowed = Vec::new();
        let mut duplicates = BTreeMap::new();
        let scoped = scopes
            .into_iter()
            .map(|(scope, candidates)| {
                let entries = candidates
                    .into_iter()
                    .map(|(meta, script, path)| RegistryEntry {
                        meta,
                        script,
                        origin: WorkflowOrigin::File(path.clone()),
                        scope,
                        path: Some(path),
                        source_label: scope.label().to_string(),
                    })
                    .collect();
                (scope, entries)
            })
            .collect();
        merge_scopes(&mut entries, &mut shadowed, &mut duplicates, scoped);
        Self {
            entries,
            shadowed,
            duplicates,
        }
    }

    pub(crate) fn resolve_by_name(&self, name: &str) -> Result<ResolvedWorkflow, RegistryError> {
        validate_name(name)?;
        if let Some((scope, sources)) = self.duplicates.get(name) {
            return Err(RegistryError::DuplicateName {
                name: name.to_string(),
                scope: *scope,
                sources: sources.clone(),
            });
        }
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.meta.name == name)
            .ok_or_else(|| RegistryError::UnknownName(name.to_string()))?;
        Ok(ResolvedWorkflow {
            meta: entry.meta.clone(),
            script: entry.script.clone(),
            origin: entry.origin.clone(),
            scope: Some(entry.scope),
        })
    }

    pub(crate) fn list(&self) -> Vec<WorkflowListing> {
        let mut listings: Vec<WorkflowListing> = self
            .entries
            .iter()
            .map(|entry| listing_for(entry, None))
            .collect();
        let winners: BTreeMap<&str, WorkflowScope> = self
            .entries
            .iter()
            .map(|entry| (entry.meta.name.as_str(), entry.scope))
            .collect();
        for entry in &self.shadowed {
            let winner = winners.get(entry.meta.name.as_str()).copied();
            listings.push(listing_for(entry, winner));
        }
        listings.sort_by(|left, right| left.name.cmp(&right.name));
        listings
    }

    // Retained for the upcoming listing surface; the tool handler formats
    // `list()` directly. Deviation D-12.
    #[allow(dead_code)]
    pub(crate) fn available_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.meta.name.clone())
            .collect()
    }
}

fn listing_for(entry: &RegistryEntry, winner: Option<WorkflowScope>) -> WorkflowListing {
    WorkflowListing {
        name: entry.meta.name.clone(),
        description: entry.meta.description.clone(),
        when_to_use: entry.meta.when_to_use.clone(),
        scope: entry.scope.label(),
        path: entry
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        shadowed_by: winner
            .filter(|winner| *winner != entry.scope)
            .map(WorkflowScope::label),
    }
}

fn merge_scopes(
    entries: &mut Vec<RegistryEntry>,
    shadowed: &mut Vec<RegistryEntry>,
    duplicates: &mut BTreeMap<String, (WorkflowScope, Vec<String>)>,
    scopes: Vec<(WorkflowScope, Vec<RegistryEntry>)>,
) {
    for (scope, mut candidates) in scopes {
        reject_same_scope_duplicates(&mut candidates, scope, duplicates);
        candidates.sort_by(|left, right| left.meta.name.cmp(&right.meta.name));
        for entry in candidates {
            if entries
                .iter()
                .any(|existing| existing.meta.name == entry.meta.name)
            {
                shadowed.push(entry);
            } else {
                entries.push(entry);
            }
        }
    }
}

fn reject_same_scope_duplicates(
    entries: &mut Vec<RegistryEntry>,
    scope: WorkflowScope,
    duplicates: &mut BTreeMap<String, (WorkflowScope, Vec<String>)>,
) {
    let mut counts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in entries.iter() {
        counts
            .entry(entry.meta.name.clone())
            .or_default()
            .push(entry.source_label.clone());
    }
    for (name, sources) in counts {
        if sources.len() > 1 {
            duplicates.insert(name.clone(), (scope, sources));
            entries.retain(|entry| entry.meta.name != name);
        }
    }
}

fn scan_directory(
    dir: &Path,
    scope: WorkflowScope,
    bundled_managed_only: bool,
) -> Vec<RegistryEntry> {
    if persist::reject_symlink(dir).is_err() {
        return Vec::new();
    }
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return Vec::new();
    };
    if !metadata.is_dir() {
        return Vec::new();
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("rhai")
                && !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with('.'))
        })
        .collect();
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    let mut entries = Vec::new();
    for path in paths {
        let Ok(script) = read_trusted_source(&path) else {
            continue;
        };
        let Ok(meta) = parse_script(&script, Some(&path)) else {
            continue;
        };
        let compiled_in = scope == WorkflowScope::Bundled
            && builtin_entries()
                .iter()
                .any(|builtin| builtin.meta.name == meta.name)
            && bundled_managed_only
            && assets::bundled_asset_is_managed(dir, &path);
        let (scope, source_label, origin) = if compiled_in {
            (
                WorkflowScope::Builtin,
                "builtin".to_string(),
                WorkflowOrigin::Builtin,
            )
        } else {
            (
                scope,
                scope.label().to_string(),
                WorkflowOrigin::File(path.clone()),
            )
        };
        entries.push(RegistryEntry {
            meta,
            script,
            origin,
            scope,
            path: Some(path),
            source_label,
        });
    }
    entries
}

pub(crate) fn resolve_inline(script: String) -> Result<ResolvedWorkflow, RegistryError> {
    if script.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(RegistryError::SourceTooLarge {
            path: "<inline>".to_string(),
            limit: MAX_WORKFLOW_SOURCE_BYTES,
        });
    }
    let meta = parse_script(&script, None)?;
    Ok(ResolvedWorkflow {
        meta,
        script,
        origin: WorkflowOrigin::Inline,
        scope: None,
    })
}

/// Resolve an explicit `script_path`. Relative paths are joined to `cwd`.
/// A path is trusted only when it lives under the project root, the user
/// workflow directory, or the session run root. Project paths additionally
/// require folder trust.
pub(crate) fn resolve_by_path(
    path: &Path,
    cwd: &Path,
    paths: &WorkflowPaths,
) -> Result<ResolvedWorkflow, RegistryError> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    read_trusted_source(&candidate)?;
    let canonical = fs::canonicalize(&candidate).map_err(|error| RegistryError::Io {
        path: candidate.display().to_string(),
        error: error.to_string(),
    })?;

    let project_root = paths
        .project_dir()
        .and_then(|dir| dir.parent())
        .and_then(|dir| dir.parent())
        .and_then(|root| fs::canonicalize(root).ok());
    let user_dir = fs::canonicalize(paths.user_dir()).ok();
    let session_runs = fs::canonicalize(paths.session_root()).ok();

    let in_session = session_runs
        .as_ref()
        .is_some_and(|root| canonical.starts_with(root));
    let in_user = user_dir
        .as_ref()
        .is_some_and(|root| canonical.starts_with(root));
    let in_project = project_root
        .as_ref()
        .is_some_and(|root| canonical.starts_with(root));

    if in_project && !in_user && !in_session && !paths.project_trusted() {
        return Err(RegistryError::UntrustedPath {
            path: candidate.display().to_string(),
            reason: "project workflows require folder trust".to_string(),
        });
    }
    if !in_project && !in_user && !in_session {
        return Err(RegistryError::UntrustedPath {
            path: candidate.display().to_string(),
            reason: "outside the project, codex home, and session workflow runs".to_string(),
        });
    }

    let script = read_trusted_source(&canonical)?;
    let filename_path = (!in_session).then_some(canonical.as_path());
    let meta = parse_script(&script, filename_path)?;
    Ok(ResolvedWorkflow {
        meta,
        script,
        origin: WorkflowOrigin::File(canonical),
        scope: None,
    })
}

/// Persist a workflow into the user scope. Create-only: an existing file is
/// never rewritten and yields `workflow_save_exists`.
pub(crate) fn save_user_workflow(
    paths: &WorkflowPaths,
    name: &str,
    script: &str,
) -> Result<PathBuf, RegistryError> {
    validate_save(name, script)?;
    let target = paths.user_dir().join(format!("{name}.rhai"));
    create_dir_all(paths.user_dir())?;
    persist::reject_symlink(paths.user_dir()).map_err(|_| RegistryError::SymlinkRejected {
        path: paths.user_dir().display().to_string(),
    })?;
    persist::create_new(&target, script.as_bytes(), MAX_WORKFLOW_SOURCE_BYTES)
        .map_err(|error| map_persist_error(error, &target))?;
    Ok(target)
}

/// Persist a workflow into the project scope. Requires a trusted project; the
/// target is confined to the project root and created without clobbering.
// Project-scope save is reserved for a future tool endpoint; app-server only
// exposes user-scope save. Deviation D-12.
#[allow(dead_code)]
pub(crate) fn save_project_workflow(
    paths: &WorkflowPaths,
    name: &str,
    script: &str,
) -> Result<PathBuf, RegistryError> {
    validate_save(name, script)?;
    if !paths.project_trusted() {
        return Err(RegistryError::UntrustedPath {
            path: paths
                .project_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "<no project>".to_string()),
            reason: "project workflows require folder trust".to_string(),
        });
    }
    let Some(project_dir) = paths.project_dir() else {
        return Err(RegistryError::UntrustedPath {
            path: "<no project>".to_string(),
            reason: "no git root is available for project workflows".to_string(),
        });
    };
    let git_root = project_dir
        .parent()
        .and_then(|dir| dir.parent())
        .ok_or_else(|| RegistryError::UntrustedPath {
            path: project_dir.display().to_string(),
            reason: "project workflow directory escaped the git root".to_string(),
        })?;
    let canonical_root = fs::canonicalize(git_root).map_err(|error| RegistryError::Io {
        path: git_root.display().to_string(),
        error: error.to_string(),
    })?;
    persist::ensure_within(&canonical_root, project_dir).map_err(|error| {
        RegistryError::UntrustedPath {
            path: project_dir.display().to_string(),
            reason: error.to_string(),
        }
    })?;
    create_dir_all(project_dir)?;
    persist::reject_symlink(project_dir).map_err(|_| RegistryError::SymlinkRejected {
        path: project_dir.display().to_string(),
    })?;
    let target = project_dir.join(format!("{name}.rhai"));
    persist::ensure_within(&canonical_root, &target).map_err(|error| {
        RegistryError::UntrustedPath {
            path: target.display().to_string(),
            reason: error.to_string(),
        }
    })?;
    persist::create_new(&target, script.as_bytes(), MAX_WORKFLOW_SOURCE_BYTES)
        .map_err(|error| map_persist_error(error, &target))?;
    Ok(target)
}

fn validate_save(name: &str, script: &str) -> Result<(), RegistryError> {
    validate_name(name)?;
    if script.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(RegistryError::SourceTooLarge {
            path: "<saved workflow>".to_string(),
            limit: MAX_WORKFLOW_SOURCE_BYTES,
        });
    }
    let meta = extract_meta(script)?;
    if meta.name != name {
        return Err(RegistryError::FilenameMismatch {
            filename: format!("{name}.rhai"),
            name: meta.name,
        });
    }
    Ok(())
}

fn create_dir_all(dir: &Path) -> Result<(), RegistryError> {
    fs::create_dir_all(dir).map_err(|error| RegistryError::Io {
        path: dir.display().to_string(),
        error: error.to_string(),
    })
}

fn map_persist_error(error: persist::PersistError, target: &Path) -> RegistryError {
    match error {
        persist::PersistError::Exists(path) => RegistryError::SaveExists { path },
        persist::PersistError::Symlink(path) => RegistryError::SymlinkRejected {
            path: path.display().to_string(),
        },
        persist::PersistError::TooLarge { max, .. } => RegistryError::SourceTooLarge {
            path: target.display().to_string(),
            limit: max,
        },
        other => RegistryError::Io {
            path: target.display().to_string(),
            error: other.to_string(),
        },
    }
}

pub(crate) fn read_trusted_source(path: &Path) -> Result<String, RegistryError> {
    let display = path.display().to_string();
    persist::reject_symlink(path).map_err(|_| RegistryError::SymlinkRejected {
        path: display.clone(),
    })?;
    let metadata = fs::metadata(path).map_err(|error| RegistryError::Io {
        path: display.clone(),
        error: error.to_string(),
    })?;
    if !metadata.is_file() {
        return Err(RegistryError::UntrustedPath {
            path: display,
            reason: "expected a regular file".to_string(),
        });
    }
    if metadata.len() > MAX_WORKFLOW_SOURCE_BYTES as u64 {
        return Err(RegistryError::SourceTooLarge {
            path: display,
            limit: MAX_WORKFLOW_SOURCE_BYTES,
        });
    }
    let file = fs::File::open(path).map_err(|error| RegistryError::Io {
        path: display.clone(),
        error: error.to_string(),
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_WORKFLOW_SOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| RegistryError::Io {
            path: display.clone(),
            error: error.to_string(),
        })?;
    if bytes.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(RegistryError::SourceTooLarge {
            path: display,
            limit: MAX_WORKFLOW_SOURCE_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|error| RegistryError::Io {
        path: display,
        error: error.to_string(),
    })
}

fn parse_script(script: &str, path: Option<&Path>) -> Result<WorkflowMeta, RegistryError> {
    let meta = extract_meta(script)?;
    validate_name(&meta.name)?;
    if let Some(path) = path {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rhai")
            || validate_name(stem).is_err()
        {
            return Err(RegistryError::InvalidFilename(filename));
        }
        if stem != meta.name {
            return Err(RegistryError::FilenameMismatch {
                filename,
                name: meta.name,
            });
        }
    }
    Ok(meta)
}

pub(crate) fn validate_name(name: &str) -> Result<(), RegistryError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(RegistryError::InvalidName(name.to_string()))
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_WORKFLOW_NAME_BYTES
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
