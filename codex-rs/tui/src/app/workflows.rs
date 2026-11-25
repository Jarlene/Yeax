//! `/workflow` and `/workflows` handling for the TUI.
//!
//! Control commands are sent over `codex-app-server-client` with the active
//! thread id so the app-server resolves the session-scoped manager on its own.
//! The TUI never depends on `codex-core` and never reads run directories.
//!
//! Starting a run has no dedicated app-server method: `/workflow <name>` submits
//! a user turn that asks the model to call the `workflow` tool, which stays the
//! single model-facing entry point.

use super::App;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use crate::history_cell::WorkflowHistoryCell;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::WorkflowControlResponse;
use codex_app_server_protocol::WorkflowErrorData;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowPauseParams;
use codex_app_server_protocol::WorkflowResumeParams;
use codex_app_server_protocol::WorkflowRunSummary;
use codex_app_server_protocol::WorkflowSaveParams;
use codex_app_server_protocol::WorkflowSaveResponse;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowUpdatedNotification;
use codex_protocol::ThreadId;
use serde_json::Map;
use serde_json::Value;
use std::fmt;
use std::time::Duration;
use std::time::Instant;
use uuid::Uuid;

pub(crate) const WORKFLOW_USAGE: &str = "Usage: /workflow <name> [--agent-budget N] [--effort LEVEL] [args] | runs | pause [run_id] | resume [run_id] | stop [run_id] | save <name> [run_id]";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct WorkflowStartArgs {
    pub name: String,
    pub agent_budget: Option<u32>,
    pub effort: Option<String>,
    /// JSON object form supplied on the command line.
    pub args: Option<Value>,
    /// Plain-text form; the design maps it to `{query, objective}`.
    pub args_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowCommand {
    Start(WorkflowStartArgs),
    Runs {
        include_completed: bool,
    },
    Pause {
        run_id: Option<String>,
    },
    Resume {
        run_id: Option<String>,
    },
    Stop {
        run_id: Option<String>,
    },
    Save {
        name: String,
        run_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowCommandError {
    Usage,
    EmptyName,
    InvalidAgentBudget(String),
    DuplicateFlag(&'static str),
    UnknownFlag(String),
    InvalidArgsJson(String),
    /// A control command omitted `run_id` while the session has no single active run.
    MissingRunId,
    /// `/workflow <name>` is a user turn, not an app-server control request.
    NotAControlRequest,
}

impl fmt::Display for WorkflowCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => formatter.write_str(WORKFLOW_USAGE),
            Self::EmptyName => formatter.write_str("workflow name must not be empty"),
            Self::InvalidAgentBudget(raw) => write!(
                formatter,
                "invalid --agent-budget {raw:?}: expected an integer in 1..=1024"
            ),
            Self::DuplicateFlag(flag) => write!(formatter, "{flag} was provided more than once"),
            Self::UnknownFlag(flag) => write!(formatter, "unknown workflow flag {flag:?}"),
            Self::InvalidArgsJson(error) => {
                write!(formatter, "workflow args must be a JSON object: {error}")
            }
            Self::MissingRunId => formatter.write_str(
                "run_id is required: pass it explicitly or keep exactly one active workflow run",
            ),
            Self::NotAControlRequest => formatter.write_str(
                "starting a workflow is submitted as a user turn, not an app-server request",
            ),
        }
    }
}

impl std::error::Error for WorkflowCommandError {}

/// Splits on whitespace while keeping JSON object/array arguments together.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if in_string {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                current.push(ch);
            }
            '{' | '[' => {
                depth += 1;
                current.push(ch);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ch if ch.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub(crate) fn parse_workflow_command(input: &str) -> Result<WorkflowCommand, WorkflowCommandError> {
    let tokens = tokenize(input.trim());
    let Some(head) = tokens.first() else {
        return Err(WorkflowCommandError::Usage);
    };
    match head.as_str() {
        "runs" | "list" => {
            if tokens.len() > 1 {
                return Err(WorkflowCommandError::Usage);
            }
            Ok(WorkflowCommand::Runs {
                include_completed: true,
            })
        }
        "pause" | "resume" | "stop" => {
            if tokens.len() > 2 {
                return Err(WorkflowCommandError::Usage);
            }
            let run_id = tokens.get(1).cloned();
            Ok(match head.as_str() {
                "pause" => WorkflowCommand::Pause { run_id },
                "resume" => WorkflowCommand::Resume { run_id },
                _ => WorkflowCommand::Stop { run_id },
            })
        }
        "save" => {
            if tokens.len() > 3 {
                return Err(WorkflowCommandError::Usage);
            }
            let Some(name) = tokens.get(1).cloned() else {
                return Err(WorkflowCommandError::Usage);
            };
            if name.is_empty() {
                return Err(WorkflowCommandError::EmptyName);
            }
            Ok(WorkflowCommand::Save {
                name,
                run_id: tokens.get(2).cloned(),
            })
        }
        _ => parse_start(&tokens),
    }
}

fn parse_start(tokens: &[String]) -> Result<WorkflowCommand, WorkflowCommandError> {
    let name = tokens[0].clone();
    if name.is_empty() {
        return Err(WorkflowCommandError::EmptyName);
    }
    let mut start = WorkflowStartArgs {
        name,
        ..Default::default()
    };
    let mut rest = Vec::new();
    let mut index = 1;
    while index < tokens.len() {
        match tokens[index].as_str() {
            "--agent-budget" => {
                if start.agent_budget.is_some() {
                    return Err(WorkflowCommandError::DuplicateFlag("--agent-budget"));
                }
                index += 1;
                let raw = tokens
                    .get(index)
                    .ok_or(WorkflowCommandError::Usage)?
                    .clone();
                let budget: u32 = raw
                    .parse()
                    .map_err(|_| WorkflowCommandError::InvalidAgentBudget(raw.clone()))?;
                if !(1..=1024).contains(&budget) {
                    return Err(WorkflowCommandError::InvalidAgentBudget(raw));
                }
                start.agent_budget = Some(budget);
            }
            "--effort" => {
                if start.effort.is_some() {
                    return Err(WorkflowCommandError::DuplicateFlag("--effort"));
                }
                index += 1;
                let raw = tokens
                    .get(index)
                    .ok_or(WorkflowCommandError::Usage)?
                    .clone();
                if raw.is_empty() {
                    return Err(WorkflowCommandError::Usage);
                }
                start.effort = Some(raw);
            }
            flag if flag.starts_with("--") => {
                return Err(WorkflowCommandError::UnknownFlag(flag.to_string()));
            }
            value => rest.push(value.to_string()),
        }
        index += 1;
    }
    let joined = rest.join(" ");
    if !joined.is_empty() {
        let trimmed = joined.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            let value: Value = serde_json::from_str(joined.trim())
                .map_err(|error| WorkflowCommandError::InvalidArgsJson(error.to_string()))?;
            if !value.is_object() {
                return Err(WorkflowCommandError::InvalidArgsJson(
                    "expected a JSON object".to_string(),
                ));
            }
            start.args = Some(value);
        } else {
            start.args_text = Some(joined);
        }
    }
    Ok(WorkflowCommand::Start(start))
}

/// Deterministic user-turn text for `/workflow <name>`.
pub(crate) fn start_prompt(start: &WorkflowStartArgs) -> String {
    let mut payload = Map::new();
    payload.insert("name".to_string(), Value::String(start.name.clone()));
    if let Some(budget) = start.agent_budget {
        payload.insert("agent_budget".to_string(), Value::from(budget));
    }
    if let Some(effort) = &start.effort {
        payload.insert("effort".to_string(), Value::String(effort.clone()));
    }
    match (&start.args, &start.args_text) {
        (Some(args), _) => {
            payload.insert("args".to_string(), args.clone());
        }
        (None, Some(text)) => {
            payload.insert(
                "args".to_string(),
                serde_json::json!({ "query": text, "objective": text }),
            );
        }
        (None, None) => {}
    }
    format!(
        "Call the `workflow` tool with {} to start this workflow.",
        Value::Object(payload)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowRequest {
    List(WorkflowListParams),
    Pause(WorkflowPauseParams),
    Resume(WorkflowResumeParams),
    Stop(WorkflowStopParams),
    Save(WorkflowSaveParams),
}

impl WorkflowRequest {
    pub(crate) fn method(&self) -> &'static str {
        match self {
            Self::List(_) => "workflow/list",
            Self::Pause(_) => "workflow/pause",
            Self::Resume(_) => "workflow/resume",
            Self::Stop(_) => "workflow/stop",
            Self::Save(_) => "workflow/save",
        }
    }

    /// Typed requests are built inline in `handle_workflow_command`; the plain
    /// conversion is used by the DTO contract tests.
    #[cfg(test)]
    pub(crate) fn into_client_request(self, request_id: RequestId) -> ClientRequest {
        match self {
            Self::List(params) => ClientRequest::WorkflowList { request_id, params },
            Self::Pause(params) => ClientRequest::WorkflowPause { request_id, params },
            Self::Resume(params) => ClientRequest::WorkflowResume { request_id, params },
            Self::Stop(params) => ClientRequest::WorkflowStop { request_id, params },
            Self::Save(params) => ClientRequest::WorkflowSave { request_id, params },
        }
    }
}

/// Builds the TASK-033 control request for one parsed command.
pub(crate) fn build_workflow_request(
    command: &WorkflowCommand,
    thread_id: &str,
    unique_active_run_id: Option<&str>,
) -> Result<WorkflowRequest, WorkflowCommandError> {
    let control_run_id = |run_id: &Option<String>| {
        run_id
            .clone()
            .or_else(|| unique_active_run_id.map(ToString::to_string))
            .ok_or(WorkflowCommandError::MissingRunId)
    };
    Ok(match command {
        WorkflowCommand::Start(_) => return Err(WorkflowCommandError::NotAControlRequest),
        WorkflowCommand::Runs { include_completed } => WorkflowRequest::List(WorkflowListParams {
            thread_id: thread_id.to_string(),
            include_completed: *include_completed,
        }),
        WorkflowCommand::Pause { run_id } => WorkflowRequest::Pause(WorkflowPauseParams {
            thread_id: thread_id.to_string(),
            run_id: control_run_id(run_id)?,
        }),
        WorkflowCommand::Resume { run_id } => WorkflowRequest::Resume(WorkflowResumeParams {
            thread_id: thread_id.to_string(),
            run_id: control_run_id(run_id)?,
        }),
        WorkflowCommand::Stop { run_id } => WorkflowRequest::Stop(WorkflowStopParams {
            thread_id: thread_id.to_string(),
            run_id: control_run_id(run_id)?,
        }),
        WorkflowCommand::Save { name, run_id } => {
            if name.is_empty() {
                return Err(WorkflowCommandError::EmptyName);
            }
            WorkflowRequest::Save(WorkflowSaveParams {
                thread_id: thread_id.to_string(),
                run_id: control_run_id(run_id)?,
                name: name.clone(),
            })
        }
    })
}

/// Renders the stable workflow error code carried by an app-server error.
///
/// Only the nine registered codes deserialize; anything else renders as
/// `unknown` so unknown server codes never leak into the transcript verbatim.
pub(crate) fn render_request_error(error: &TypedRequestError) -> Vec<String> {
    match error {
        TypedRequestError::Server { method, source } => {
            let data = source
                .data
                .as_ref()
                .and_then(|value| serde_json::from_value::<WorkflowErrorData>(value.clone()).ok());
            let code = data
                .as_ref()
                .map(|data| data.code.as_str())
                .unwrap_or("unknown");
            let mut lines = vec![format!("{method} failed: {code} ({})", source.message)];
            if let Some(detail) = data.and_then(|data| data.detail) {
                lines.push(format!("detail: {detail}"));
            }
            lines
        }
        TypedRequestError::Transport { method, source } => {
            vec![format!("{method} transport error: {source}")]
        }
        TypedRequestError::Deserialize { method, source } => {
            vec![format!("{method} response decode error: {source}")]
        }
    }
}

enum WorkflowRequestOutcome {
    List(WorkflowListResponse),
    Control(WorkflowControlResponse),
    Saved(WorkflowSaveResponse),
}

impl App {
    /// `/workflow ...` and `/workflows` entry point.
    pub(super) async fn handle_workflow_command(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        command: WorkflowCommand,
    ) {
        let active_run_id = self
            .workflow_cards
            .unique_active_run_id()
            .map(ToString::to_string);
        let request = match build_workflow_request(
            &command,
            &thread_id.to_string(),
            active_run_id.as_deref(),
        ) {
            Ok(request) => request,
            Err(error) => {
                self.chat_widget.add_error_message(error.to_string());
                return;
            }
        };
        let method = request.method();
        let request_id = RequestId::String(format!(
            "workflow-{}-{}",
            method.replace('/', "-"),
            Uuid::new_v4()
        ));
        let request_handle = app_server.request_handle();
        let result = tokio::time::timeout(REQUEST_TIMEOUT, async move {
            match request {
                WorkflowRequest::List(params) => request_handle
                    .request_typed::<WorkflowListResponse>(ClientRequest::WorkflowList {
                        request_id,
                        params,
                    })
                    .await
                    .map(WorkflowRequestOutcome::List),
                WorkflowRequest::Pause(params) => request_handle
                    .request_typed::<WorkflowControlResponse>(ClientRequest::WorkflowPause {
                        request_id,
                        params,
                    })
                    .await
                    .map(WorkflowRequestOutcome::Control),
                WorkflowRequest::Resume(params) => request_handle
                    .request_typed::<WorkflowControlResponse>(ClientRequest::WorkflowResume {
                        request_id,
                        params,
                    })
                    .await
                    .map(WorkflowRequestOutcome::Control),
                WorkflowRequest::Stop(params) => request_handle
                    .request_typed::<WorkflowControlResponse>(ClientRequest::WorkflowStop {
                        request_id,
                        params,
                    })
                    .await
                    .map(WorkflowRequestOutcome::Control),
                WorkflowRequest::Save(params) => request_handle
                    .request_typed::<WorkflowSaveResponse>(ClientRequest::WorkflowSave {
                        request_id,
                        params,
                    })
                    .await
                    .map(WorkflowRequestOutcome::Saved),
            }
        })
        .await;

        let now = Instant::now();
        let lines = match result {
            Ok(Ok(WorkflowRequestOutcome::List(response))) => {
                self.workflow_cards.apply_summaries(&response.runs, now);
                let cards = self.workflow_cards.cards();
                let cell = WorkflowHistoryCell::dashboard(&cards, now);
                self.insert_workflow_cell(cell);
                return;
            }
            Ok(Ok(WorkflowRequestOutcome::Control(response))) => {
                self.workflow_cards
                    .apply_summaries(std::slice::from_ref(&response.run), now);
                vec![control_summary_line(&response.run)]
            }
            Ok(Ok(WorkflowRequestOutcome::Saved(response))) => {
                vec![format!("saved workflow to {} (user scope)", response.path)]
            }
            Ok(Err(error)) => render_request_error(&error),
            Err(_) => vec![format!(
                "{method} timed out after {}s",
                REQUEST_TIMEOUT.as_secs()
            )],
        };
        self.insert_workflow_cell(WorkflowHistoryCell::from_strings(lines));
    }

    /// Consumes `workflow/updated`; duplicate revisions produce no history row.
    pub(super) fn apply_workflow_update(&mut self, notification: &WorkflowUpdatedNotification) {
        let now = Instant::now();
        if crate::history_cell::WorkflowCardChange::Applied
            != self.workflow_cards.apply_update(notification, now)
        {
            return;
        }
        if let Some(card) = self.workflow_cards.card(&notification.run_id) {
            let cell = WorkflowHistoryCell::from_card(card, now);
            self.insert_workflow_cell(cell);
        }
    }

    fn insert_workflow_cell(&mut self, cell: WorkflowHistoryCell) {
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
    }
}

fn control_summary_line(run: &WorkflowRunSummary) -> String {
    let phase = run.phase.as_deref().unwrap_or("-");
    format!(
        "workflow {} {} [{}] phase={} agents={}",
        run.run_id, run.name, run.status, phase, run.agent_count
    )
}

#[cfg(test)]
#[path = "workflows_tests.rs"]
mod tests;
