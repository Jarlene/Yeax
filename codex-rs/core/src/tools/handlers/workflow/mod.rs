//! Workflow tool handlers.
//!
//! `error` owns the stable error-code contract; the run/control/description
//! handlers translate tool arguments into `workflow::request_service` requests
//! and map typed failures back through `WorkflowToolError`.

pub(crate) mod control;
pub(crate) mod description;
pub(crate) mod error;
pub(crate) mod run;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub(crate) use run::WorkflowHandler;
