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
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
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
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum JobKind {
    Mutation,
    Action,
}

/// What to run once a job has finished ([`ScheduledJob::on_complete`]): the mutation at `path`, with args
/// `{ workId, context, result }` ([`crate::scheduler::WorkResult`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct OnComplete {
    pub path: String,
    #[ts(type = "any")]
    pub context: Value,
}

/// How long to wait before retrying a failed action: `initial_ms`, then `initial_ms * base`, `* base²`, ... (at most
/// an hour).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct Backoff {
    pub initial_ms: f64,
    pub base: f64,
}

/// A scheduled run of a mutation or an action (`_scheduled_jobs`). Created by `ctx.scheduler.run_after` /
/// `run_at`, by crons, and by the dashboard's "run now" on a finished job. The TS types are inlined (`#[ts(type)]`)
/// so app code can return `Id<ScheduledJob>` (`Id<"_scheduled_jobs">`) without declaring extra named types.
#[table(name = "_scheduled_jobs")]
#[index(by_state_run_at = [state, run_at, scheduled_at])]
#[index(by_completed_at = [completed_at])]
#[index(by_kind_state_run_at = [kind, state, run_at, scheduled_at])]
#[index(by_kind_pool_state_run_at = [kind, pool, state, run_at, scheduled_at])]
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
    // Who scheduled it: a function path, "cron:<name>", "dashboard", "onComplete:<job id>" or "workflow:<id>".
    pub origin: Option<String>,
    // The workpool whose parallelism limits this action (`RxFerrix::workpool`); null for the default pool
    // (`SchedulerOptions::concurrency`). Mutations run one at a time whatever their pool.
    pub pool: Option<String>,
    // The mutation run once the job has finished (success, failed or canceled), as a job of its own.
    #[ts(type = "{ path: string, context: any } | null")]
    pub on_complete: Option<OnComplete>,
    // The retry backoff of this action; null for the scheduler's (`SchedulerOptions::retry_backoff`, doubling).
    #[ts(type = "{ initialMs: number, base: number } | null")]
    pub backoff: Option<Backoff>,
    // How much of each retry delay is random, 0 to 1 (`ScheduleWith::jitter`); null for the scheduler's
    // (`SchedulerOptions::retry_jitter`).
    pub jitter: Option<f64>,
}

/// A cron declared with `RxFerrix::cron` / `RxFerrix::cron_tz` / `RxFerrix::interval` (`_crons`), reconciled at startup:
/// rows of crons that are no longer declared are deleted. Names starting with `_` are rxferrix's own system crons.
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

// Unit enums: their variant names (the analytics copy's column is text).
impl crate::shape::HasShape for JobKind {
    fn shape() -> crate::shape::Shape {
        crate::shape::Shape::Text
    }
}

impl crate::shape::HasShape for JobState {
    fn shape() -> crate::shape::Shape {
        crate::shape::Shape::Text
    }
}
