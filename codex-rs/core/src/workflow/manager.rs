//! Run lifecycle surface shared with tools and app-server.
//!
//! This module owns the typed control error contract consumed by TASK-031 and
//! the deterministic task-name rules from design 03 section 6.2. The
//! orchestrator itself lives in `run_manager`; the error variants below are the
//! authoritative source and must not be duplicated by callers.

use std::fmt;
use std::path::PathBuf;

use codex_config::config_toml::WorkflowConfigToml;

use super::paths::WorkflowPathError;
use super::tracker::RunStatus;

/// Design 03 section 5.3: at most four active runs per session.
pub(crate) const MAX_ACTIVE_RUNS: usize = 4;

/// Default agent-call budget per run (design 03 section 5.7).
pub(crate) const DEFAULT_AGENT_BUDGET: u64 = 128;

/// Readability bound from design 03 section 6.2; not a protocol limit.
pub(crate) const MAX_TASK_NAME_BYTES: usize = 48;

/// Runtime limits resolved from `[workflow]` in `config.toml`.
///
/// Design 03 section 5.7 keeps these at their P0 defaults while still allowing
/// explicit overrides, so the config values must reach the manager rather than
/// living as dead accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkflowLimits {
    pub max_active_runs: usize,
    pub default_agent_budget: u64,
    pub max_concurrent_agents: u16,
}

impl Default for WorkflowLimits {
    fn default() -> Self {
        Self {
            max_active_runs: MAX_ACTIVE_RUNS,
            default_agent_budget: DEFAULT_AGENT_BUDGET,
            max_concurrent_agents: 32,
        }
    }
}

impl WorkflowLimits {
    /// Resolve validated config into effective runtime limits.
    ///
    /// `WorkflowConfigToml::validate` is expected to have run first; the
    /// `effective_*` accessors clamp to the documented defaults.
    pub(crate) fn from_config(config: Option<&WorkflowConfigToml>) -> Self {
        match config {
            Some(config) => Self {
                max_active_runs: usize::from(config.effective_max_active_runs()),
                default_agent_budget: config.effective_default_agent_budget(),
                max_concurrent_agents: config.effective_max_concurrent_agents(),
            },
            None => Self::default(),
        }
    }
}

#[derive(Debug)]
pub enum WorkflowControlError {
    Paths(WorkflowPathError),
    Unavailable(String),
    Internal(String),
    /// `[features] workflows` is off for the turn that tried to launch.
    Disabled,
    /// The run record or frozen inputs could not be published.
    LaunchFailed(String),
    RunNotFound(String),
    NotApplicable {
        run_id: String,
        status: RunStatus,
        allowed: Vec<&'static str>,
    },
    MaxActiveRunsExceeded {
        active: usize,
        max: usize,
    },
    /// Used by the save path (app-server only; not a tool control endpoint).
    InvalidName(String),
    /// Used by the save path (app-server only).
    SaveExists(PathBuf),
    /// Used by the save path (app-server only); `detail` keeps the errno text.
    SaveFailed {
        path: PathBuf,
        detail: String,
    },
}

impl fmt::Display for WorkflowControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Paths(error) => write!(f, "{error}"),
            Self::Unavailable(detail) => write!(f, "workflow manager unavailable: {detail}"),
            Self::Internal(detail) => write!(f, "workflow internal error: {detail}"),
            Self::Disabled => write!(
                f,
                "background workflows are disabled; enable `[features] workflows = true`"
            ),
            Self::LaunchFailed(detail) => write!(f, "workflow failed to launch: {detail}"),
            Self::RunNotFound(run_id) => write!(f, "workflow run not found: {run_id}"),
            Self::NotApplicable {
                run_id,
                status,
                allowed,
            } => write!(
                f,
                "workflow run {run_id} is {status:?}; allowed: {}",
                allowed.join(", ")
            ),
            Self::MaxActiveRunsExceeded { active, max } => {
                write!(f, "too many active workflow runs ({active} > {max})")
            }
            Self::InvalidName(name) => write!(f, "workflow name invalid: {name}"),
            Self::SaveExists(path) => {
                write!(f, "workflow already exists: {}", path.display())
            }
            Self::SaveFailed { path, detail } => {
                write!(f, "workflow save failed at {}: {detail}", path.display())
            }
        }
    }
}

impl std::error::Error for WorkflowControlError {}

impl From<WorkflowPathError> for WorkflowControlError {
    fn from(error: WorkflowPathError) -> Self {
        Self::Paths(error)
    }
}

/// Deterministic task-name normalization from design 03 section 6.2.
///
/// The caller supplies an already-selected source string: `opts.label` when
/// present, otherwise the first 32 characters of the prompt.
pub(crate) fn normalize_task_name(source: &str, used: &[String]) -> String {
    // Path-shaped inputs (`root`, `.`, `..`) lose every byte to normalization,
    // so they are mapped explicitly instead of falling into the `agent`
    // fallback. Design 03 section 6.2 requires the `wf_` prefix for all three
    // and distinct results so one run cannot collapse them into one name.
    let path_shaped = match source.trim() {
        "root" => Some("wf_root"),
        "." => Some("wf_dot"),
        ".." => Some("wf_dotdot"),
        _ => None,
    };
    if let Some(fixed) = path_shaped {
        return dedupe(fixed.to_string(), used);
    }
    let normalized = sanitize(source);
    dedupe(normalized, used)
}

fn sanitize(source: &str) -> String {
    let mut normalized = String::with_capacity(source.len().min(MAX_TASK_NAME_BYTES));
    let mut last_underscore = false;
    for byte in source.bytes() {
        let mapped = if byte.is_ascii_alphanumeric() {
            byte.to_ascii_lowercase() as char
        } else {
            '_'
        };
        if mapped == '_' {
            if last_underscore || normalized.is_empty() {
                continue;
            }
            last_underscore = true;
        } else {
            last_underscore = false;
        }
        normalized.push(mapped);
    }
    while normalized.ends_with('_') {
        normalized.pop();
    }
    if normalized.is_empty() {
        normalized.push_str("agent");
    }
    truncate_bytes(&mut normalized, MAX_TASK_NAME_BYTES);
    while normalized.ends_with('_') {
        normalized.pop();
    }
    if normalized.is_empty() {
        normalized.push_str("agent");
    }
    normalized
}

/// Same-run uniqueness: `name`, `name_2`, `name_3`, ...
fn dedupe(normalized: String, used: &[String]) -> String {
    if !used.iter().any(|name| name == &normalized) {
        return normalized;
    }
    for suffix in 2u32.. {
        // The suffix has to fit *inside* the byte bound. Appending first and
        // truncating afterwards would fold `_2` back onto the same 48-byte
        // base and loop forever on a name that already fills the budget.
        let suffix_text = format!("_{suffix}");
        let mut base = normalized.clone();
        truncate_bytes(
            &mut base,
            MAX_TASK_NAME_BYTES.saturating_sub(suffix_text.len()),
        );
        while base.ends_with('_') {
            base.pop();
        }
        if base.is_empty() {
            base.push_str("agent");
        }
        let candidate = format!("{base}{suffix_text}");
        if !used.iter().any(|name| name == &candidate) {
            return candidate;
        }
    }
    normalized
}

fn truncate_bytes(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut boundary = max;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// Tracks names already handed out inside one run.
#[derive(Debug, Default)]
pub(crate) struct TaskNames {
    used: Vec<String>,
}

impl TaskNames {
    pub(crate) fn next(&mut self, source: &str) -> String {
        let name = normalize_task_name(source, &self.used);
        self.used.push(name.clone());
        name
    }

    // Reserved for host-side name diagnostics. Deviation D-12.
    #[allow(dead_code)]
    pub(crate) fn used(&self) -> &[String] {
        &self.used
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
