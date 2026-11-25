//! Parses and routes workflow tool requests. Launch/validate/control are kept
//! apart so a `pause`/`stop` can never accidentally create a run.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use codex_workflow::MAX_AGENT_BUDGET;
use codex_workflow::PhaseMeta;
use serde::Deserialize;
use serde_json::Value;

use super::assets::MAX_TEMPLATE_BYTES;
use super::paths::WorkflowPaths;
use super::registry::RegistryError;
use super::registry::ResolvedWorkflow;
use super::registry::WorkflowRegistry;
use super::registry::read_trusted_source;
use super::registry::resolve_by_path;
use super::registry::resolve_inline;
use super::registry::save_user_workflow;

pub(crate) const DEFAULT_AGENT_BUDGET: u64 = 128;
pub(crate) const MIN_AGENT_BUDGET: u64 = 1;
// P2 Grok import is explicit-only and not yet exposed; deviation D-16.
#[allow(dead_code)]
pub(crate) const MAX_GROK_IMPORT_BYTES: u64 = MAX_TEMPLATE_BYTES as u64;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WorkflowSource {
    Name {
        name: String,
    },
    Script {
        script: String,
    },
    ScriptPath {
        script_path: String,
    },
    Resume {
        resume_from_run_id: String,
    },
    Pause {
        #[serde(default)]
        run_id: Option<String>,
    },
    Stop {
        #[serde(default)]
        run_id: Option<String>,
    },
}

impl WorkflowSource {
    pub(crate) fn is_control(&self) -> bool {
        matches!(self, Self::Pause { .. } | Self::Stop { .. })
    }

    pub(crate) fn is_resume(&self) -> bool {
        matches!(self, Self::Resume { .. })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct WorkflowRequest {
    pub source: WorkflowSource,
    #[serde(default)]
    pub agent_budget: Option<u64>,
    #[serde(default)]
    pub args: Option<Value>,
    #[serde(default)]
    pub validate_only: bool,
}

impl WorkflowRequest {
    /// Parameter-level validation. Feature gating happens before this and is
    /// reported as `workflows_disabled`.
    pub(crate) fn validate(&self) -> Result<(), RequestError> {
        if let Some(budget) = self.agent_budget
            && !(MIN_AGENT_BUDGET..=MAX_AGENT_BUDGET).contains(&budget)
        {
            return Err(RequestError::invalid_input(format!(
                "agent_budget must be {MIN_AGENT_BUDGET}..={MAX_AGENT_BUDGET}, got {budget}"
            )));
        }
        if self.source.is_control() {
            if self.agent_budget.is_some() {
                return Err(RequestError::invalid_input(
                    "agent_budget is not accepted for pause/stop",
                ));
            }
            if self.args.is_some() {
                return Err(RequestError::invalid_input(
                    "args is not accepted for pause/stop",
                ));
            }
            if self.validate_only {
                return Err(RequestError::invalid_input(
                    "validate_only is not accepted for pause/stop",
                ));
            }
        }
        if self.source.is_resume() && self.validate_only {
            return Err(RequestError::invalid_input(
                "validate_only is not accepted for resume",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlAction {
    Pause,
    Stop,
}

#[derive(Debug, Clone)]
pub(crate) enum PreparedRequest {
    Validate {
        workflow: ResolvedWorkflow,
        args: Value,
        agent_budget: u64,
    },
    Launch {
        workflow: ResolvedWorkflow,
        args: Value,
        agent_budget: Option<u64>,
        resume_run_id: Option<String>,
    },
    Control {
        action: ControlAction,
        run_id: String,
    },
}

#[derive(Debug)]
pub(crate) struct RequestError {
    code: &'static str,
    detail: String,
}

impl RequestError {
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    fn invalid_input(detail: impl Into<String>) -> Self {
        Self {
            code: "workflow_invalid_input",
            detail: detail.into(),
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            code: "workflows_disabled",
            detail: "Background workflows are disabled for this session \
                     ([features] workflows = true / config flag)."
                .to_string(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn not_available() -> Self {
        Self {
            code: "workflow_not_available",
            detail: "workflow manager is not initialized for this session".to_string(),
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for RequestError {}

impl From<RegistryError> for RequestError {
    fn from(error: RegistryError) -> Self {
        let code = match &error {
            RegistryError::UnknownName(_) => "workflow_not_found",
            other => other.code(),
        };
        Self {
            code,
            detail: error.to_string(),
        }
    }
}

/// Read-only view of run state needed to resolve `resume` and the
/// "unique active candidate" shorthand for `pause`/`stop`.
pub(crate) trait RunScriptLookup {
    /// Exact run id or unambiguous run-id prefix.
    fn find_run_id(&self, key: &str) -> Option<String>;
    /// Frozen `script.rhai` of a run.
    fn script_copy_for(&self, run_id: &str) -> Option<String>;
    /// Frozen `args.json` of a run (used by reference semantics).
    fn args_copy_for(&self, run_id: &str) -> Option<Value>;
    /// Non-terminal run ids, in creation order.
    fn active_run_ids(&self) -> Vec<String>;
}

pub(crate) struct RequestService<'a> {
    registry: &'a WorkflowRegistry,
    runs: &'a dyn RunScriptLookup,
    paths: Arc<WorkflowPaths>,
    cwd: PathBuf,
    enabled: bool,
    default_agent_budget: u64,
}

impl<'a> RequestService<'a> {
    pub(crate) fn new(
        registry: &'a WorkflowRegistry,
        runs: &'a dyn RunScriptLookup,
        paths: Arc<WorkflowPaths>,
        cwd: PathBuf,
        enabled: bool,
    ) -> Self {
        Self {
            registry,
            runs,
            paths,
            cwd,
            enabled,
            default_agent_budget: DEFAULT_AGENT_BUDGET,
        }
    }

    /// Adopt `[workflow] default_agent_budget` instead of the built-in default.
    pub(crate) fn with_default_agent_budget(mut self, budget: u64) -> Self {
        self.default_agent_budget = budget;
        self
    }

    pub(crate) fn prepare(
        &self,
        request: &WorkflowRequest,
    ) -> Result<PreparedRequest, RequestError> {
        if !self.enabled {
            return Err(RequestError::disabled());
        }
        request.validate()?;

        match &request.source {
            WorkflowSource::Pause { run_id } => Ok(PreparedRequest::Control {
                action: ControlAction::Pause,
                run_id: self.resolve_control_run(run_id.as_deref())?,
            }),
            WorkflowSource::Stop { run_id } => Ok(PreparedRequest::Control {
                action: ControlAction::Stop,
                run_id: self.resolve_control_run(run_id.as_deref())?,
            }),
            WorkflowSource::Resume { resume_from_run_id } => {
                let run_id =
                    self.runs
                        .find_run_id(resume_from_run_id)
                        .ok_or_else(|| RequestError {
                            code: "workflow_resume_unknown_run",
                            detail: format!(
                                "no workflow run with a persisted script matches \
                             {resume_from_run_id:?} in this session"
                            ),
                        })?;
                let script = self
                    .runs
                    .script_copy_for(&run_id)
                    .ok_or_else(|| RequestError {
                        code: "workflow_resume_unknown_run",
                        detail: format!("workflow run {run_id} has no persisted script"),
                    })?;
                let workflow = resolve_inline(script)?;
                let args = self.runs.args_copy_for(&run_id).unwrap_or(Value::Null);
                Ok(PreparedRequest::Launch {
                    workflow,
                    args,
                    agent_budget: request.agent_budget,
                    resume_run_id: Some(run_id),
                })
            }
            source => {
                let workflow = match source {
                    WorkflowSource::Name { name } => self.registry.resolve_by_name(name)?,
                    WorkflowSource::Script { script } => resolve_inline(script.clone())?,
                    WorkflowSource::ScriptPath { script_path } => {
                        resolve_by_path(Path::new(script_path), &self.cwd, &self.paths)?
                    }
                    _ => unreachable!("control and resume are handled above"),
                };
                let args = request.args.clone().unwrap_or(Value::Null);
                if request.validate_only {
                    Ok(PreparedRequest::Validate {
                        workflow,
                        args,
                        agent_budget: request.agent_budget.unwrap_or(self.default_agent_budget),
                    })
                } else {
                    Ok(PreparedRequest::Launch {
                        workflow,
                        args,
                        agent_budget: request.agent_budget,
                        resume_run_id: None,
                    })
                }
            }
        }
    }

    fn resolve_control_run(&self, explicit: Option<&str>) -> Result<String, RequestError> {
        if let Some(key) = explicit {
            return self.runs.find_run_id(key).ok_or_else(|| RequestError {
                code: "workflow_control_unknown_run",
                detail: format!("no workflow run matches {key:?} in this session"),
            });
        }
        let active = self.runs.active_run_ids();
        match active.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(RequestError {
                code: "workflow_control_unknown_run",
                detail: "no active workflow run to control; pass run_id".to_string(),
            }),
            many => Err(RequestError::invalid_input(format!(
                "run_id is required when more than one run is active: {}",
                many.join(", ")
            ))),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct GrokImportReport {
    pub imported: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub rejected: Vec<(String, String)>,
}

/// P2 explicit migration of `~/.grok/workflows/*.rhai`. Never runs implicitly:
/// the caller must invoke this function, and nothing outside `grok_dir` is read.
#[allow(dead_code)]
pub(crate) fn import_grok(
    grok_dir: &Path,
    paths: &WorkflowPaths,
) -> Result<GrokImportReport, RequestError> {
    let mut report = GrokImportReport::default();
    let Ok(metadata) = std::fs::symlink_metadata(grok_dir) else {
        return Ok(report);
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RequestError::invalid_input(format!(
            "grok import source must be a real directory: {}",
            grok_dir.display()
        )));
    }
    let Ok(read_dir) = std::fs::read_dir(grok_dir) else {
        return Ok(report);
    };
    let mut paths_to_import: Vec<PathBuf> = read_dir
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rhai"))
        .collect();
    paths_to_import.sort();

    for path in paths_to_import {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = (|| {
            let script = read_trusted_source(&path)?;
            if script.len() as u64 > MAX_GROK_IMPORT_BYTES {
                return Err(RegistryError::SourceTooLarge {
                    path: path.display().to_string(),
                    limit: MAX_GROK_IMPORT_BYTES as usize,
                });
            }
            let meta = codex_workflow::extract_meta(&script)?;
            super::registry::validate_name(&meta.name)?;
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if stem != meta.name {
                return Err(RegistryError::FilenameMismatch {
                    filename: file_name.clone(),
                    name: meta.name,
                });
            }
            Ok::<_, RegistryError>((meta.name, script))
        })();
        match result {
            Ok((name, script)) => match save_user_workflow(paths, &name, &script) {
                Ok(_) => report.imported.push(name),
                Err(RegistryError::SaveExists { .. }) => report.skipped_existing.push(name),
                Err(error) => report.rejected.push((file_name, error.to_string())),
            },
            Err(error) => report.rejected.push((file_name, error.to_string())),
        }
    }
    Ok(report)
}

/// Phase metadata surfaced by a successful validation.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) struct ValidationSummary {
    pub name: String,
    pub phases: Vec<PhaseMeta>,
}

#[cfg(test)]
#[path = "request_service_tests.rs"]
mod tests;
