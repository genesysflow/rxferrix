//! The scheduler's system tables: `_scheduled_jobs` (one row per scheduled run) and `_crons` (one row per declared
//! cron). Ordinary tables (docs/CONTRACT.md §4), so the dashboard and subscriptions see them live.
//!
//! Field comments are plain `//` comments: ts-rs would copy doc comments into the generated `SystemDataModel`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prelude::*;

/// Where a job is in its life: `pending` → (`inProgress`, actions only) → `success` | `failed`, or `canceled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum JobState {
    /// Waiting for its `runAt`.
    Pending,
    /// An action that is running. Mutation jobs never show this state: they go from pending to success or failed
    /// in one transaction.
    InProgress,
    Success,
    Failed,
    Canceled,
}

impl JobState {
    /// Success, failed or canceled: the job will not run (again).
    pub fn is_finished(self) -> bool {
        matches!(self, JobState::Success | JobState::Failed | JobState::Canceled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::InProgress => "inProgress",
            JobState::Success => "success",
            JobState::Failed => "failed",
            JobState::Canceled => "canceled",
        }
    }
}

/// What kind of function a job runs (recorded at scheduling time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum JobKind {
    Mutation,
    Action,
}

/// A scheduled run of a mutation or an action (`_scheduled_jobs`). Created by `ctx.scheduler.run_after` /
/// `run_at`, by crons, and by the dashboard's "run now" on a finished job. The TS types are inlined (`#[ts(type)]`)
/// so app code can return `Id<ScheduledJob>` (`Id<"_scheduled_jobs">`) without declaring extra named types.
#[table(name = "_scheduled_jobs")]
#[index(by_state_run_at = [state, run_at, scheduled_at])]
#[index(by_completed_at = [completed_at])]
pub struct ScheduledJob {
    // The function's path, e.g. "todos:add".
    pub path: String,
    #[ts(type = "\"mutation\" | \"action\"")]
    pub kind: JobKind,
    // The args, serialized when the job was scheduled.
    #[ts(type = "any")]
    pub args: Value,
    // When the job is due (ms since the epoch). Retries of actions move it forward.
    pub run_at: f64,
    #[ts(type = "\"pending\" | \"inProgress\" | \"success\" | \"failed\" | \"canceled\"")]
    pub state: JobState,
    // Runs started so far.
    pub attempts: u32,
    // 1, or 1 + retries for actions scheduled `with_retries(n)`.
    pub max_attempts: u32,
    // The return value as JSON text, cut to `MAX_OUTCOME_BYTES`.
    pub result: Option<String>,
    // The (last) error message, cut to `MAX_OUTCOME_BYTES`.
    pub error: Option<String>,
    // When it was scheduled: the document's `_creationTime`, strictly increasing, so it orders jobs due at the
    // same time.
    pub scheduled_at: f64,
    pub started_at: Option<f64>,
    // Set once the job is success, failed or canceled.
    pub completed_at: Option<f64>,
    // Who scheduled it: a function path, "cron:<name>", or "dashboard".
    pub origin: Option<String>,
}

/// A cron declared with `Ferrix::cron` / `Ferrix::cron_tz` / `Ferrix::interval` (`_crons`), reconciled at startup:
/// rows of crons that are no longer declared are deleted. Names starting with `_` are ferrix's own system crons.
#[table(name = "_crons")]
#[index(by_name = [name])]
#[index(by_next_run = [next_run])]
pub struct Cron {
    pub name: String,
    // The cron expression ("0 */5 * * * *"), or "every <duration>" for intervals.
    pub schedule: String,
    // The IANA time zone the expression is evaluated in; null for UTC.
    pub timezone: Option<String>,
    pub path: String,
    #[ts(type = "any")]
    pub args: Value,
    // When it last enqueued a job (ms since the epoch).
    pub last_run: Option<f64>,
    pub next_run: f64,
    // The job it enqueued last; while that one is pending or in progress, runs are skipped.
    pub last_job: Option<Id<ScheduledJob>>,
}
