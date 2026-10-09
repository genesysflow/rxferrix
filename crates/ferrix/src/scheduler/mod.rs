//! The scheduler and crons (PLAN.md §7.3).
//!
//! - [`Scheduler`] (`ctx.scheduler` in mutations and actions): `run_after`, `run_at`, `cancel`, `with_retries`.
//! - [`tables`]: the system tables `_scheduled_jobs` ([`ScheduledJob`]) and `_crons` ([`Cron`]).
//! - [`runner`]: the background runner (`App::start_scheduler`, started by `ferrix serve`) and
//!   `App::run_due_jobs` for tests. Its module docs describe the delivery guarantees: **mutations run exactly
//!   once, actions at most once**.
//! - [`cron`]: crons declared with `Ferrix::cron` / `Ferrix::cron_tz` / `Ferrix::interval` ([`CronSpec`]), time zones,
//!   and ferrix's own system crons (`_storage_sweep`).
//! - [`cancellation`]: canceling running actions (`ActionCtx::is_canceled` / `canceled`, the abort after
//!   `SchedulerOptions::cancel_grace`).
//!
//! The functions below are system functions (paths `_system/scheduler:<name>`): two internal mutations that
//! `ctx.scheduler` uses from actions (`schedule`, `cancel`), and the admin-only functions behind the dashboard's
//! Scheduler page (`jobs`, `cancel_job`, `run_job_now`, `crons`, `run_cron_now`).

pub mod cancellation;
pub mod context;
pub mod cron;
pub mod store;
pub mod runner;
pub mod tables;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use context::{ScheduleWith, Scheduler};
pub use cancellation::ActionAborted;
pub use cron::{CronSpec, SWEEP_CRON};
pub use store::CancelOutcome;
pub use runner::SchedulerState;
pub use tables::{Cron, JobKind, JobState, ScheduledJob};

use crate::SchedulableFunction;
use crate::prelude::*;
use store::NewJob;

/// Scheduler settings: `AppOptions::scheduler`, or `Ferrix::scheduler` / `Ferrix::cron` / `Ferrix::interval`.
#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    /// Jobs running at the same time (each running action holds a slot; mutations run one at a time anyway).
    /// Default 10.
    pub concurrency: usize,
    /// Finished (success, failed, canceled) jobs are deleted this long after they finished. `None` keeps them.
    /// Default 7 days.
    pub retention: Option<Duration>,
    /// How often the retention cleanup runs. Default 1 hour.
    pub cleanup_interval: Duration,
    /// Delay before the first retry of an action scheduled `with_retries(n)`; doubled for each further retry (at
    /// most 1 hour). Default 1 second.
    pub retry_backoff: Duration,
    /// How long a canceled action may keep running after its job was canceled before it is aborted (dropped at its
    /// next `.await`). Default 5 seconds. See [`cancellation`].
    pub cancel_grace: Duration,
    /// The declared crons.
    pub crons: Vec<CronSpec>,
}

/// Default [`SchedulerOptions::concurrency`].
pub const DEFAULT_CONCURRENCY: usize = 10;
/// Default [`SchedulerOptions::retention`].
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default [`SchedulerOptions::cancel_grace`].
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(5);

impl Default for SchedulerOptions {
    fn default() -> Self {
        SchedulerOptions {
            concurrency: DEFAULT_CONCURRENCY,
            retention: Some(DEFAULT_RETENTION),
            cleanup_interval: Duration::from_secs(3600),
            retry_backoff: Duration::from_secs(1),
            cancel_grace: DEFAULT_CANCEL_GRACE,
            crons: Vec::new(),
        }
    }
}

impl SchedulerOptions {
    /// Declare a cron: see [`CronSpec::cron`].
    pub fn cron<F: SchedulableFunction>(mut self, name: impl Into<String>, expr: impl Into<String>, args: impl Into<F::Args>) -> Self {
        self.crons.push(CronSpec::cron::<F>(name, expr, args));
        self
    }

    /// Declare a cron evaluated in an IANA time zone: see [`CronSpec::cron_tz`].
    pub fn cron_tz<F: SchedulableFunction>(
        mut self,
        name: impl Into<String>,
        expr: impl Into<String>,
        tz: impl Into<String>,
        args: impl Into<F::Args>,
    ) -> Self {
        self.crons.push(CronSpec::cron_tz::<F>(name, expr, tz, args));
        self
    }

    /// Declare an interval cron: see [`CronSpec::interval`].
    pub fn interval<F: SchedulableFunction>(mut self, name: impl Into<String>, every: Duration, args: impl Into<F::Args>) -> Self {
        self.crons.push(CronSpec::interval::<F>(name, every, args));
        self
    }
}

tokio::task_local! {
    /// Set while a scheduled job runs, so the execution log names the caller `scheduler`.
    pub(crate) static IN_JOB: ();
}

/// The execution log's caller for anonymous executions inside a scheduled job (and the functions it calls).
pub(crate) fn log_caller() -> Option<String> {
    IN_JOB.try_with(|_| ()).ok().map(|()| "scheduler".to_string())
}

fn user_error(code: &str, message: impl Into<String>) -> Error {
    UserError::new(code, message).into()
}

fn parse_job_id(id: &str) -> Result<Id<ScheduledJob>> {
    id.parse().map_err(|_| user_error("invalid_id", format!("`{id}` is not a scheduled job id")))
}

// ---------------------------------------------------------------------------------------------------------------------
// Internal mutations behind `ctx.scheduler` in actions
// ---------------------------------------------------------------------------------------------------------------------

/// Schedule `path` (a mutation or action) with JSON `args` at `run_at`. What `ctx.scheduler.run_after` / `run_at`
/// run from an action.
#[internal_mutation]
pub async fn schedule(
    ctx: &MutationCtx,
    path: String,
    args: Value,
    run_at: f64,
    max_attempts: u32,
    origin: Option<String>,
) -> Result<Id<ScheduledJob>> {
    let kind = store::kind_of(&path)?;
    let max_attempts = if kind == JobKind::Action { max_attempts.max(1) } else { 1 };
    store::insert(&ctx.db, NewJob { path, kind, args, run_at, max_attempts, origin }, ctx.now()).await
}

/// Cancel a job (see `Scheduler::cancel`), recording `reason` (default: canceled by an action). What
/// `ctx.scheduler.cancel` runs from an action.
#[internal_mutation]
pub async fn cancel(ctx: &MutationCtx, id: Id<ScheduledJob>, reason: Option<String>) -> Result<CancelOutcome> {
    let reason = reason.unwrap_or_else(|| "Canceled by an action".to_string());
    store::cancel(&ctx.db, id, ctx.now(), &reason).await
}

// ---------------------------------------------------------------------------------------------------------------------
// Dashboard (admin only)
// ---------------------------------------------------------------------------------------------------------------------

/// Scheduled jobs, live and paginated. With `state`, only jobs in that state, through the `(state, runAt)` index:
/// pending and in-progress jobs soonest first, finished ones most recent first. Without, every job, newest first.
#[query]
pub async fn jobs(ctx: &QueryCtx, state: Option<JobState>, pagination_opts: PaginationOpts) -> Result<PaginationResult<Doc<ScheduledJob>>> {
    ctx.auth.require_admin()?;
    let query = match state {
        Some(state) => {
            let order = if state.is_finished() { Order::Desc } else { Order::Asc };
            ctx.db
                .query::<ScheduledJob>()
                .with_index(ScheduledJob::by_state_run_at, |q| q.eq(ScheduledJob::state, state))
                .order(order)
        }
        None => ctx.db.query::<ScheduledJob>().order(Order::Desc),
    };
    query.paginate(pagination_opts).await
}

/// Cancel a pending job or a running action (signaled, then aborted after the grace period; see
/// `Scheduler::cancel`). Returns what happened: `canceled` or `alreadyFinished`.
#[mutation]
pub async fn cancel_job(ctx: &MutationCtx, id: String) -> Result<CancelOutcome> {
    ctx.auth.require_admin()?;
    let who = ctx.auth.identity().log_summary().unwrap_or_else(|| "an admin".to_string());
    store::cancel(&ctx.db, parse_job_id(&id)?, ctx.now(), &format!("Canceled from the dashboard by {who}")).await
}

/// Run a job now. A pending job becomes due immediately (same id). A finished job (success, failed, canceled) is
/// run again as a new job with the same function and args (origin `dashboard`). Returns the id of the job that will
/// run. An action in progress can't be run again until it finishes (`running`).
#[mutation]
pub async fn run_job_now(ctx: &MutationCtx, id: String) -> Result<String> {
    ctx.auth.require_admin()?;
    let id = parse_job_id(&id)?;
    let Some(job) = ctx.db.get(id).await? else {
        return Err(user_error("not_found", format!("Scheduled job {id} does not exist")));
    };
    let now = ctx.now();
    match job.state {
        JobState::Pending => {
            let mut value = job.value;
            value.run_at = value.run_at.min(now);
            ctx.db.replace(id, value).await?;
            Ok(id.to_string())
        }
        JobState::InProgress => Err(user_error("running", "This job is running; try again once it has finished")),
        _ => {
            let kind = store::kind_of(&job.path)?;
            let job = NewJob {
                path: job.value.path,
                kind,
                args: job.value.args,
                run_at: now,
                max_attempts: job.value.max_attempts,
                origin: Some("dashboard".to_string()),
            };
            Ok(store::insert(&ctx.db, job, now).await?.to_string())
        }
    }
}

/// A declared cron with its last and next run.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct CronSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    pub name: String,
    pub schedule: String,
    /// The IANA time zone of the expression; `null` for UTC.
    pub timezone: Option<String>,
    /// A ferrix system cron (name starting with `_`, e.g. `_storage_sweep`), not declared by the app.
    pub system: bool,
    pub path: String,
    pub args: Value,
    pub last_run: Option<f64>,
    /// `null` when the schedule has no future run.
    pub next_run: Option<f64>,
    pub last_job: Option<String>,
    pub last_job_state: Option<JobState>,
}

/// Every declared cron with the state of its last job: the app's by name, then the system crons. Live.
#[query]
pub async fn crons(ctx: &QueryCtx) -> Result<Vec<CronSummary>> {
    ctx.auth.require_admin()?;
    let mut rows = ctx.db.query::<Cron>().collect().await?;
    rows.sort_by(|a, b| (a.name.starts_with('_'), &a.name).cmp(&(b.name.starts_with('_'), &b.name)));
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let last_job_state = match row.last_job {
            Some(job) => ctx.db.get(job).await?.map(|j| j.state),
            None => None,
        };
        out.push(CronSummary {
            id: row.id.to_string(),
            system: row.name.starts_with('_'),
            name: row.value.name,
            schedule: row.value.schedule,
            timezone: row.value.timezone,
            path: row.value.path,
            args: row.value.args,
            last_run: row.value.last_run,
            next_run: Some(row.value.next_run).filter(|t| *t < f64::MAX),
            last_job: row.value.last_job.map(|j| j.to_string()),
            last_job_state,
        });
    }
    Ok(out)
}

/// Run a cron now ("run now" on the dashboard): enqueue its job, due immediately, with origin `cron:<name>`, and make
/// it the cron's `lastRun` / `lastJob` (its `nextRun` is unchanged). The no-pile-up rule applies: while the cron's
/// previous job is still pending or in progress this fails with the user error `running`, unless `force`. Returns
/// the new job's id. `not_found` for an unknown cron name; system crons (e.g. `_storage_sweep`) can be run too.
#[mutation]
pub async fn run_cron_now(ctx: &MutationCtx, name: String, force: Option<bool>) -> Result<String> {
    ctx.auth.require_admin()?;
    let row = ctx.db.query::<Cron>().with_index(Cron::by_name, |q| q.eq(Cron::name, name.clone())).first().await?;
    let Some(row) = row else {
        return Err(user_error("not_found", format!("No cron is named `{name}`")));
    };
    match cron::enqueue_now(&ctx.db, row, ctx.now(), force.unwrap_or(false)).await? {
        Some(job) => Ok(job.to_string()),
        None => Err(user_error("running", format!("The previous run of `{name}` hasn't finished; pass force to run it anyway"))),
    }
}
