//! Compiled-in workflow assets, bundled installation, and prompt templates.
//!
//! `assets/generated.rs` is the only generated file. It is checked in and
//! verified by `codex-workflow-assets-gen --check`; do not edit it by hand.

#[path = "assets/generated.rs"]
mod generated;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::persist;
use super::persist::PersistError;

pub(crate) use generated::EmbeddedAsset;

pub(crate) const MAX_TEMPLATE_BYTES: usize = 1_048_576;
pub(crate) const BUNDLED_INDEX_VERSION: u8 = 1;

pub(crate) fn builtin_scripts() -> &'static [EmbeddedAsset] {
    generated::builtin_workflows()
}

pub(crate) fn bundled_scripts() -> &'static [EmbeddedAsset] {
    generated::bundled_workflows()
}

pub(crate) fn bundled_prompts() -> &'static [EmbeddedAsset] {
    generated::bundled_prompts()
}

pub(crate) fn template_keys() -> &'static [&'static str] {
    generated::TEMPLATE_KEYS
}

pub(crate) fn template(name: &str) -> Option<&'static str> {
    generated::template(name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderTemplateError {
    UnknownTemplate(String),
    TemplateTooLarge { actual: usize, max: usize },
    RenderedTooLarge { actual: usize, max: usize },
}

impl fmt::Display for RenderTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTemplate(name) => write!(f, "unknown template: {name}"),
            Self::TemplateTooLarge { .. } => {
                write!(f, "template exceeds {MAX_TEMPLATE_BYTES} bytes")
            }
            Self::RenderedTooLarge { .. } => {
                write!(f, "rendered template exceeds {MAX_TEMPLATE_BYTES} bytes")
            }
        }
    }
}

impl std::error::Error for RenderTemplateError {}

/// Render a template by replacing single-brace `{key}` placeholders.
///
/// Unknown keys and malformed braces stay verbatim. Strings are inserted as-is;
/// every other JSON value uses its compact JSON representation.
pub(crate) fn render_template(name: &str, vars: &Value) -> Result<String, RenderTemplateError> {
    match template(name) {
        Some(source) => render_template_source(source, vars),
        None => {
            let available = template_keys().join(", ");
            tracing::warn!(template = name, available, "unknown workflow template");
            Err(RenderTemplateError::UnknownTemplate(name.to_string()))
        }
    }
}

fn render_template_source(source: &str, vars: &Value) -> Result<String, RenderTemplateError> {
    if source.len() > MAX_TEMPLATE_BYTES {
        return Err(RenderTemplateError::TemplateTooLarge {
            actual: source.len(),
            max: MAX_TEMPLATE_BYTES,
        });
    }
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(open) = rest.find('{') {
        let (before, after_open) = rest.split_at(open);
        out.push_str(before);
        let after = &after_open[1..];
        let Some(close) = after.find('}') else {
            out.push_str(after_open);
            rest = "";
            break;
        };
        let key = &after[..close];
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            out.push('{');
            rest = after;
            continue;
        }
        match lookup(vars, key) {
            Some(value) => out.push_str(&stringify(value)),
            None => {
                out.push('{');
                out.push_str(key);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    if out.len() > MAX_TEMPLATE_BYTES {
        return Err(RenderTemplateError::RenderedTooLarge {
            actual: out.len(),
            max: MAX_TEMPLATE_BYTES,
        });
    }
    Ok(out)
}

fn lookup<'a>(vars: &'a Value, key: &str) -> Option<&'a Value> {
    let mut current = vars;
    for segment in key.split('.') {
        current = current.get(segment)?;
    }
    if current.is_null() {
        None
    } else {
        Some(current)
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InstallReport {
    pub installed: usize,
    pub upgraded: usize,
    pub up_to_date: usize,
    pub skipped_modified: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum InstallError {
    Persist(PersistError),
    ReadOnly { path: PathBuf },
    InvalidIndex { path: PathBuf, reason: String },
    LockTimeout { path: PathBuf },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Persist(error) => write!(f, "{error}"),
            Self::ReadOnly { path } => write!(
                f,
                "{}: workflow assets directory is read-only",
                path.display()
            ),
            Self::InvalidIndex { path, reason } => {
                write!(f, "{}: invalid bundled index: {reason}", path.display())
            }
            Self::LockTimeout { path } => write!(
                f,
                "{}: timed out waiting for the bundled install lock",
                path.display()
            ),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<PersistError> for InstallError {
    fn from(error: PersistError) -> Self {
        Self::Persist(error)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BundledIndex {
    version: u8,
    #[serde(default)]
    entries: BTreeMap<String, String>,
}

/// True while `path` is byte-identical to the hash recorded for it by the
/// bundled installer. A user-edited copy loses its managed (builtin-privileged)
/// status without being rewritten.
pub(crate) fn bundled_asset_is_managed(bundled_dir: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Ok(index) = read_index(&bundled_dir.join(".bundled-index.json")) else {
        return false;
    };
    let Some(expected) = index.entries.get(name) else {
        return false;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    sha256_hex(&bytes) == *expected
}

/// Install or upgrade compiled-in bundled assets into `bundled_dir`.
///
/// - Missing targets are created with create-only semantics.
/// - A target that still matches the recorded index hash is upgraded atomically.
/// - A target modified by the user is left untouched and reported as a warning,
///   so a local edit never disappears silently.
pub(crate) fn ensure_bundled_assets(bundled_dir: &Path) -> Result<InstallReport, InstallError> {
    fs::create_dir_all(bundled_dir).map_err(|source| {
        if source.kind() == std::io::ErrorKind::PermissionDenied {
            InstallError::ReadOnly {
                path: bundled_dir.to_path_buf(),
            }
        } else {
            InstallError::Persist(PersistError::Io {
                path: bundled_dir.to_path_buf(),
                source,
            })
        }
    })?;
    let _lock = InstallLock::acquire(bundled_dir)?;

    let index_path = bundled_dir.join(".bundled-index.json");
    let mut index = read_index(&index_path)?;
    index.version = BUNDLED_INDEX_VERSION;
    let mut report = InstallReport::default();

    for asset in bundled_scripts() {
        let file_name = asset.path.rsplit('/').next().unwrap_or(asset.path);
        install_one(
            bundled_dir,
            file_name,
            asset.content,
            asset.sha256,
            &index,
            &mut report,
        )?;
        index
            .entries
            .insert(file_name.to_string(), asset.sha256.to_string());
    }
    for asset in bundled_prompts() {
        install_one(
            bundled_dir,
            asset.path,
            asset.content,
            asset.sha256,
            &index,
            &mut report,
        )?;
        index
            .entries
            .insert(asset.path.to_string(), asset.sha256.to_string());
    }

    let bytes = serde_json::to_vec_pretty(&index).map_err(|error| InstallError::InvalidIndex {
        path: index_path.clone(),
        reason: error.to_string(),
    })?;
    persist::atomic_write(&index_path, &bytes, 1024 * 1024)?;
    Ok(report)
}

fn install_one(
    root: &Path,
    relative: &str,
    content: &str,
    sha256: &str,
    index: &BundledIndex,
    report: &mut InstallReport,
) -> Result<(), InstallError> {
    let target = root.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            InstallError::Persist(PersistError::Io {
                path: parent.to_path_buf(),
                source,
            })
        })?;
    }
    match fs::read(&target) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            match persist::create_new(&target, content.as_bytes(), MAX_TEMPLATE_BYTES) {
                Ok(()) => report.installed += 1,
                Err(error) if error.is_exists() => report.up_to_date += 1,
                Err(error) => return Err(error.into()),
            }
        }
        Err(source) => {
            return Err(InstallError::Persist(PersistError::Io {
                path: target,
                source,
            }));
        }
        Ok(existing) => {
            let current_sha = sha256_hex(&existing);
            if current_sha == sha256 {
                report.up_to_date += 1;
            } else if index
                .entries
                .get(relative)
                .is_some_and(|recorded| recorded == &current_sha)
            {
                persist::atomic_write(&target, content.as_bytes(), MAX_TEMPLATE_BYTES)?;
                report.upgraded += 1;
            } else {
                report.skipped_modified += 1;
                report.warnings.push(format!(
                    "{}: bundled asset was modified locally; leaving it untouched",
                    target.display()
                ));
            }
        }
    }
    Ok(())
}

fn read_index(path: &Path) -> Result<BundledIndex, InstallError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BundledIndex {
                version: BUNDLED_INDEX_VERSION,
                entries: BTreeMap::new(),
            });
        }
        Err(source) => {
            return Err(InstallError::Persist(PersistError::Io {
                path: path.to_path_buf(),
                source,
            }));
        }
    };
    match serde_json::from_slice::<BundledIndex>(&bytes) {
        Ok(mut index) if index.version == BUNDLED_INDEX_VERSION => {
            index.version = BUNDLED_INDEX_VERSION;
            Ok(index)
        }
        Ok(index) => Ok(BundledIndex {
            version: BUNDLED_INDEX_VERSION,
            entries: index.entries,
        }),
        Err(error) => Err(InstallError::InvalidIndex {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

struct InstallLock {
    path: PathBuf,
    _file: fs::File,
}

impl InstallLock {
    fn acquire(dir: &Path) -> Result<Self, InstallError> {
        let path = dir.join(".bundled-index.lock");
        let deadline = SystemTime::now() + Duration::from_secs(5);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self { path, _file: file });
                }
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    clear_stale_lock(&path);
                    if SystemTime::now() >= deadline {
                        return Err(InstallError::LockTimeout { path });
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(source) => {
                    return Err(InstallError::Persist(PersistError::Io { path, source }));
                }
            }
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn clear_stale_lock(path: &Path) {
    let stale_after = Duration::from_secs(60);
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let Ok(modified) = meta.modified() else {
        return;
    };
    if SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > stale_after)
    {
        let _ = fs::remove_file(path);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
#[path = "assets_tests.rs"]
mod tests;
