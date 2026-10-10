//! The workflow system tables: `_workflows` (one row per workflow run) and `_workflow_steps` (its journal: one row
//! per step). Ordinary tables, so the dashboard and subscriptions see them live.
//!
//! Field comments are plain `//` comments: ts-rs would copy doc comments into the generated `SystemDataModel`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prelude::*;
use crate::scheduler::{OnComplete, ScheduledJob};

/// Where a workflow is: `running` → `success` | `failed` | `canceled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum WorkflowState {
    Running,
    Success,
    Failed,
    Canceled,
}

impl WorkflowState {
    pub fn is_finished(self) -> bool {
        self != WorkflowState::Running
    }
}

/// An event sent to a workflow before it waited for it (`Workflows::send_event`); the next `wait_for_event` of
/// that name takes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct PendingEvent {
    pub name: String,
    #[ts(type = "any")]
    pub value: Value,
}

/// A run of a `#[workflow]` (`_workflows`), started with `ctx.workflows.start::<W>(args)`. The TS types are inlined
/// so app code can return `Id<Workflow>` (`Id<"_workflows">`) without declaring extra named types.
#[table(name = "_workflows")]
#[index(by_state = [state, started_at])]
#[index(by_completed_at = [completed_at])]
pub struct Workflow {
    // The workflow's path, e.g. "onboarding:onboard".
    pub name: String,
    #[ts(type = "any")]
    pub args: Value,
    #[ts(type = "\"running\" | \"success\" | \"failed\" | \"canceled\"")]
    pub state: WorkflowState,
    // The return value (success).
    #[ts(type = "any")]
    pub result: Option<Value>,
    // Why it failed or was canceled.
    pub error: Option<String>,
    // Steps started so far.
    pub steps: u32,
    pub started_at: f64,
    pub completed_at: Option<f64>,
    // Who started it: a function path, or "dashboard".
    pub origin: Option<String>,
    // The mutation run once it has finished, as a scheduled job (args `{ workflowId, context, result }`).
    #[ts(type = "{ path: string, context: any } | null")]
    pub on_complete: Option<OnComplete>,
    // Events sent before the workflow waited for them, oldest first.
    #[ts(type = "Array<{ name: string, value: any }>")]
    pub events: Vec<PendingEvent>,
}

/// What a step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum StepKind {
    Query,
    Mutation,
    Action,
    Sleep,
    Event,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Query => "query",
            StepKind::Mutation => "mutation",
            StepKind::Action => "action",
            StepKind::Sleep => "sleep",
            StepKind::Event => "event",
        }
    }
}

/// Where a step is: `running` (its job is pending or running, the sleep or the event hasn't come) → `success` |
/// `failed` | `canceled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum StepState {
    Running,
    Success,
    Failed,
    Canceled,
}

/// One step of a workflow (`_workflow_steps`): the journal its body is replayed against.
#[table(name = "_workflow_steps")]
#[index(by_workflow = [workflow, index])]
pub struct WorkflowStep {
    pub workflow: Id<Workflow>,
    // 0, 1, 2, ... in the order the body started its steps.
    pub index: u32,
    #[ts(type = "\"query\" | \"mutation\" | \"action\" | \"sleep\" | \"event\"")]
    pub kind: StepKind,
    // The function's path, the event's name, or "sleep".
    pub name: String,
    // The function's args; `{ ms }` for a sleep; null for an event.
    #[ts(type = "any")]
    pub args: Value,
    #[ts(type = "\"running\" | \"success\" | \"failed\" | \"canceled\"")]
    pub state: StepState,
    // The return value (the event's value), in full: the body gets it back on every replay.
    #[ts(type = "any")]
    pub result: Option<Value>,
    pub error: Option<String>,
    // The scheduled job that runs it (mutations, actions) or ends it (sleeps).
    pub job: Option<Id<ScheduledJob>>,
    pub started_at: f64,
    pub completed_at: Option<f64>,
}
