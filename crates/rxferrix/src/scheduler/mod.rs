//! The scheduler and crons (docs/CONTRACT.md §1.7).
//!
//! - [`Scheduler`] (`ctx.scheduler` in mutations and actions): `run_after`, `run_at`, `cancel`, `with_retries`, and
//!   workpools: `pool(name)` (bounded parallelism per pool, [`Workpool`]), `on_complete::<F>(context)` (a mutation
//!   run with the outcome, [`WorkResult`]), `cancel_pool`.
//! - [`tables`]: the system tables `_scheduled_jobs` ([`ScheduledJob`]) and `_crons` ([`Cron`]).
//! - [`history`]: the job history in `aux.db`, where finished jobs move after [`SchedulerOptions::history_after`].
//! - [`runner`]: the background runner (`App::start_scheduler`, started by `rxferrix serve`) and
//!   `App::run_due_jobs` for tests. Its module docs describe the delivery guarantees: **mutations run exactly
//!   once, actions at most once**.
//! - [`cron`]: crons declared with `RxFerrix::cron` / `RxFerrix::cron_tz` / `RxFerrix::interval` ([`CronSpec`]), time zones,
//!   and rxferrix's own system crons (`_storage_sweep`).
//! - [`cancellation`]: canceling running actions (`ActionCtx::is_canceled` / `canceled`, the abort after
//!   `SchedulerOptions::cancel_grace`).
//!
//! The functions below are system functions (paths `_system/scheduler:<name>`): two internal mutations that
//! `ctx.scheduler` uses from actions (`schedule`, `cancel`), and the admin-only functions behind the dashboard's
//! Scheduler page (`jobs`, `cancel_job`, `run_job_now`, `crons`, `run_cron_now`).

pub mod cancellation;
pub mod context;
pub mod cron;
pub mod history;
pub mod runner;
pub mod store;
pub mod tables;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use cancellation::ActionAborted;
pub use context::{ScheduleWith, Scheduler};
pub use cron::{CronSpec, SWEEP_CRON};
pub use history::HistoryFilter;
pub use runner::SchedulerState;
pub use store::{CancelOutcome, WorkResult};
pub use tables::{Backoff, Cron, JobKind, JobState, OnComplete, ScheduledJob};

use crate::SchedulableFunction;
use crate::prelude::*;
use store::NewJob;

/// Scheduler settings: `AppOptions::scheduler`, or `RxFerrix::scheduler` / `RxFerrix::cron` / `RxFerrix::interval`.
#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    /// Actions of the default pool (scheduled without [`Scheduler::pool`]) running at the same time. Mutations
    /// run one at a time anyway, and need no slot. Default 10.
    pub concurrency: usize,
    /// Finished (success, failed, canceled) jobs move from `_scheduled_jobs` to the job history in `aux.db`
    /// ([`history`]) this long after they finished, by the cleanup. `None` keeps them in `_scheduled_jobs` until
    /// `retention`. Default 1 hour.
    pub history_after: Option<Duration>,
    /// Finished jobs (in `_scheduled_jobs` or the job history) and finished workflows are deleted this long after
    /// they finished. `None` keeps them. Default 7 days.
    pub retention: Option<Duration>,
    /// How often the cleanup (history and retention) runs. Default 1 hour.
    pub cleanup_interval: Duration,
    /// Delay before the first retry of an action scheduled `with_retries(n)`; doubled for each further retry (at
    /// most 1 hour). Default 1 second.
    pub retry_backoff: Duration,
    /// How much of each retry delay is random, from 0 (none) to 1 (anywhere between no wait and the full delay):
    /// a retry waits between `1 - retry_jitter` and 1 times its backoff. Spreads out retries of actions that failed
    /// together. For actions that don't set their own ([`Workpool::jitter`], `ScheduleWith::jitter`). Default 0.
    pub retry_jitter: f64,
    /// How long a canceled action may keep running after its job was canceled before it is aborted (dropped at its
    /// next `.await`). Default 5 seconds. See [`cancellation`].
    pub cancel_grace: Duration,
    /// The declared crons.
    pub crons: Vec<CronSpec>,
    /// The declared workpools.
    pub pools: Vec<Workpool>,
}

/// A workpool: a named limit on how many of its actions run at once, with default retries for them. Declared with
/// `RxFerrix::workpool(Workpool::new("emails").max_parallelism(5).retries(3))`; used with
/// `ctx.scheduler.pool("emails").enqueue::<send_email>(args)`. Each pool has its own slots: its actions don't take
/// the default pool's ([`SchedulerOptions::concurrency`]), nor another pool's. Within a pool, actions start in the
/// order they are due.
#[derive(Debug, Clone)]
pub struct Workpool {
    pub name: String,
    /// Actions of this pool running at the same time. Default 10.
    pub max_parallelism: usize,
    /// Retries of a failing action enqueued in this pool, unless it says otherwise (`with_retries`). Default 0.
    pub retries: u32,
    /// The retry backoff of its actions. Default: the scheduler's (`SchedulerOptions::retry_backoff`, doubling).
    pub backoff: Option<Backoff>,
    /// The retry jitter of its actions. Default: the scheduler's (`SchedulerOptions::retry_jitter`).
    pub jitter: Option<f64>,
}

impl Workpool {
    pub fn new(name: impl Into<String>) -> Workpool {
        Workpool { name: name.into(), max_parallelism: DEFAULT_CONCURRENCY, retries: 0, backoff: None, jitter: None }
    }

    pub fn max_parallelism(mut self, n: usize) -> Workpool {
        self.max_parallelism = n;
        self
    }

    pub fn retries(mut self, n: u32) -> Workpool {
        self.retries = n;
        self
    }

    /// Wait `initial` before the first retry, then `base` times longer before each further one (at most an hour).
    pub fn backoff(mut self, initial: Duration, base: f64) -> Workpool {
        self.backoff = Some(Backoff { initial_ms: initial.as_secs_f64() * 1000.0, base });
        self
    }

    /// Make each retry wait a random `1 - jitter` to 1 times its backoff (`jitter` from 0 to 1), so actions that
    /// failed together don't all retry at once. See [`SchedulerOptions::retry_jitter`].
    pub fn jitter(mut self, jitter: f64) -> Workpool {
        self.jitter = Some(jitter);
        self
    }
}

/// Default [`SchedulerOptions::concurrency`].
pub const DEFAULT_CONCURRENCY: usize = 10;
/// Default [`SchedulerOptions::history_after`].
pub const DEFAULT_HISTORY_AFTER: Duration = Duration::from_secs(3600);
/// Default [`SchedulerOptions::retention`].
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default [`SchedulerOptions::cancel_grace`].
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(5);

impl Default for SchedulerOptions {
    fn default() -> Self {
        SchedulerOptions {
            concurrency: DEFAULT_CONCURRENCY,
            history_after: Some(DEFAULT_HISTORY_AFTER),
            retention: Some(DEFAULT_RETENTION),
            cleanup_interval: Duration::from_secs(3600),
            retry_backoff: Duration::from_secs(1),
            retry_jitter: 0.0,
            cancel_grace: DEFAULT_CANCEL_GRACE,
            crons: Vec::new(),
            pools: Vec::new(),
        }
    }
}

impl SchedulerOptions {
    /// Declare a cron: see [`CronSpec::cron`].
    pub fn cron<F: SchedulableFunction>(
        mut self,
        name: impl Into<String>,
        expr: impl Into<String>,
        args: impl Into<F::Args>,
    ) -> Self {
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

    /// Declare a workpool: see [`Workpool`].
    pub fn workpool(mut self, pool: Workpool) -> Self {
        self.pools.push(pool);
        self
    }

    /// Declare an interval cron: see [`CronSpec::interval`].
    pub fn interval<F: SchedulableFunction>(
        mut self,
        name: impl Into<String>,
        every: Duration,
        args: impl Into<F::Args>,
    ) -> Self {
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
    IN_JOB.try_with(|_| ()).ok().map(|()| crate::exec_log::SCHEDULER.to_string())
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
/// run from an action (which has already checked `pool` and `on_complete`).
#[internal_mutation]
#[allow(clippy::too_many_arguments)]
pub async fn schedule(
    ctx: &MutationCtx,
    path: String,
    args: Value,
    run_at: f64,
    max_attempts: u32,
    origin: Option<String>,
    pool: Option<String>,
    on_complete: Option<OnComplete>,
    backoff: Option<Backoff>,
    jitter: Option<f64>,
) -> Result<Id<ScheduledJob>> {
    let kind = store::kind_of(&path)?;
    let max_attempts = if kind == JobKind::Action { max_attempts.max(1) } else { 1 };
    let job = NewJob { path, kind, args, run_at, max_attempts, origin, pool, on_complete, backoff, jitter };
    store::insert(&ctx.db, job, ctx.now()).await
}

/// Cancel every pending job and running action of a workpool (see `Scheduler::cancel_pool`). What
/// `ctx.scheduler.cancel_pool` runs from an action.
#[internal_mutation]
pub async fn cancel_pool(ctx: &MutationCtx, pool: String, reason: String) -> Result<u64> {
    store::cancel_pool(&ctx.db, &pool, ctx.now(), &reason).await
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
/// Finished jobs are here until they move to the job history ([`SchedulerOptions::history_after`]).
#[query]
pub async fn jobs(
    ctx: &QueryCtx,
    state: Option<JobState>,
    pagination_opts: PaginationOpts,
) -> Result<PaginationResult<Doc<ScheduledJob>>> {
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
        _ => Ok(store::insert(&ctx.db, rerun(job.value, now)?, now).await?.to_string()),
    }
}

/// A new job, due `now`, running the same function with the same args as the finished `job` (origin `dashboard`).
pub(crate) fn rerun(job: ScheduledJob, now: f64) -> Result<NewJob> {
    Ok(NewJob {
        kind: store::kind_of(&job.path)?,
        path: job.path,
        args: job.args,
        run_at: now,
        max_attempts: job.max_attempts,
        origin: Some("dashboard".to_string()),
        pool: job.pool,
        on_complete: job.on_complete,
        backoff: job.backoff,
        jitter: job.jitter,
    })
}

/// A declared cron with its last and next run.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct CronSummary {
    #[serde(rename = "_id")]
    #[ts(rename = "_id")]
    pub id: String,
    pub name: String,
    pub schedule: String,
    /// The IANA time zone of the expression; `null` for UTC.
    pub timezone: Option<String>,
    /// A rxferrix system cron (name starting with `_`, e.g. `_storage_sweep`), not declared by the app.
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
        None => Err(user_error(
            "running",
            format!("The previous run of `{name}` hasn't finished; pass force to run it anyway"),
        )),
    }
}
