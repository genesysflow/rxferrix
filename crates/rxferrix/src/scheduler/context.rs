//! `ctx.scheduler`: schedule mutations and actions from mutations (transactionally) and from actions (immediately).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::Workpool;
use super::store::{self, NewJob};
use super::tables::{Backoff, JobKind, OnComplete, ScheduledJob};
use crate::db::WriteTxn;
use crate::db::query::MutationDb;
use crate::id::Id;
use crate::runtime::{App, Identity};
use crate::{MutationFunction, SchedulableFunction};

/// Schedules functions to run later: `ctx.scheduler` in mutations and actions.
///
/// ```ignore
/// #[mutation]
/// pub async fn send(ctx: &MutationCtx, body: String) -> Result<Id<Message>> {
///     let id = ctx.db.insert(Message { body }).await?;
///     ctx.scheduler.run_after::<moderate>(Duration::ZERO, (id,)).await?;   // runs only if `send` commits
///     Ok(id)
/// }
/// ```
///
/// - In a **mutation**, the job is written in the mutation's own transaction: it exists only if the mutation
///   commits, and a `cancel` is just as atomic.
/// - In an **action**, scheduling and canceling take effect immediately (each runs the internal system mutation
///   `_system/scheduler:schedule` / `:cancel`).
///
/// The target is any mutation or action, internal ones included ([`SchedulableFunction`]); scheduling a query does
/// not compile. Options go through [`ScheduleWith`]: `ctx.scheduler.pool("emails").with_retries(3)
/// .on_complete::<sent>(context).enqueue::<send_email>(args)` (see [`Workpool`] and
/// [`crate::scheduler::WorkResult`]). Args are serialized when the job is scheduled. Scheduled functions run with no
/// user identity (`ctx.auth` is anonymous; the execution log shows the caller as `scheduler`). Mutations run exactly
/// once; actions at most once, see [`crate::scheduler`].
pub struct Scheduler {
    target: Target,
    /// The scheduling function's path, recorded as the job's `origin`.
    origin: String,
    /// The declared workpools.
    pools: Arc<BTreeMap<String, Workpool>>,
}

enum Target {
    Mutation(Arc<WriteTxn>),
    Action { app: App, identity: Identity },
}

/// When a job is due.
#[derive(Clone, Copy)]
enum When {
    After(Duration),
    At(f64),
}

impl When {
    fn resolve(self, now: f64) -> f64 {
        match self {
            When::After(d) => now + d.as_secs_f64() * 1000.0,
            When::At(t) => t,
        }
    }
}

impl Scheduler {
    pub(crate) fn in_mutation(txn: Arc<WriteTxn>, origin: &str, pools: Arc<BTreeMap<String, Workpool>>) -> Scheduler {
        Scheduler { target: Target::Mutation(txn), origin: origin.to_string(), pools }
    }

    pub(crate) fn in_action(app: App, identity: Identity, origin: &str) -> Scheduler {
        let pools = app.scheduler_state().pools().clone();
        Scheduler { target: Target::Action { app, identity }, origin: origin.to_string(), pools }
    }

    /// The same scheduler, recording `origin` as the jobs' origin.
    pub(crate) fn with_origin(&self, origin: String) -> Scheduler {
        let target = match &self.target {
            Target::Mutation(txn) => Target::Mutation(txn.clone()),
            Target::Action { app, identity } => Target::Action { app: app.clone(), identity: identity.clone() },
        };
        Scheduler { target, origin, pools: self.pools.clone() }
    }

    /// Run `F` with `args` once `delay` has passed (from this mutation's `now()`, or from now in an action).
    /// Returns the job's id (for [`Scheduler::cancel`]).
    pub async fn run_after<F: SchedulableFunction>(
        &self,
        delay: Duration,
        args: impl Into<F::Args>,
    ) -> crate::Result<Id<ScheduledJob>> {
        self.options().run_after::<F>(delay, args).await
    }

    /// Run `F` with `args` at `at_ms` (ms since the epoch; a time in the past means "as soon as possible").
    pub async fn run_at<F: SchedulableFunction>(
        &self,
        at_ms: f64,
        args: impl Into<F::Args>,
    ) -> crate::Result<Id<ScheduledJob>> {
        self.options().run_at::<F>(at_ms, args).await
    }

    fn options(&self) -> ScheduleWith<'_> {
        ScheduleWith { scheduler: self, retries: None, pool: None, backoff: None, on_complete: None }
    }

    /// Opt into retries for an **action**: if it returns an error, it is run again up to `retries` more times, with
    /// exponential backoff (`SchedulerOptions::retry_backoff`, doubled each time, or [`ScheduleWith::backoff`]).
    /// Ignored for mutations. An action interrupted by a restart is retried while attempts remain; external side
    /// effects may repeat, so retryable actions need an idempotency strategy.
    ///
    /// `ctx.scheduler.with_retries(3).run_after::<send_email>(Duration::ZERO, (to,)).await?`
    pub fn with_retries(&self, retries: u32) -> ScheduleWith<'_> {
        self.options().with_retries(retries)
    }

    /// Run in workpool `name` (declared with `RxFerrix::workpool`): at most its `max_parallelism` of its actions run at
    /// once, and they get its default retries. See [`Workpool`].
    ///
    /// `ctx.scheduler.pool("emails").enqueue::<send_email>((to,)).await?`
    pub fn pool(&self, name: &str) -> ScheduleWith<'_> {
        self.options().pool(name)
    }

    /// Once the job has finished (succeeded, failed for good, or been canceled), run mutation `F` with
    /// `{ workId, context, result }` ([`crate::scheduler::WorkResult`]). See [`ScheduleWith::on_complete`].
    pub fn on_complete<F: MutationFunction>(&self, context: impl serde::Serialize) -> ScheduleWith<'_> {
        self.options().on_complete::<F>(context)
    }

    /// Cancel a job. A pending job becomes `canceled` and won't run. An action in progress becomes `canceled` too and
    /// is signaled ([`ActionCtx::is_canceled`](crate::ActionCtx::is_canceled) / `canceled()`); if it hasn't stopped
    /// `SchedulerOptions::cancel_grace` (5s) later, it is aborted at its next `.await` (mutations it already ran stay
    /// committed). Canceling a finished job does nothing. Fails with the user error `not_found` for an unknown id.
    /// Inside a mutation, the cancel commits (or rolls back) with it, and a running action is signaled only after the
    /// commit. The job's `error` records who canceled it (`Canceled by <this function's path>`).
    pub async fn cancel(&self, id: Id<ScheduledJob>) -> crate::Result<()> {
        self.cancel_with_reason(id, &format!("Canceled by {}", self.origin)).await
    }

    /// [`Scheduler::cancel`], recording `reason` as the job's `error` instead.
    pub async fn cancel_with_reason(&self, id: Id<ScheduledJob>, reason: &str) -> crate::Result<()> {
        match &self.target {
            Target::Mutation(txn) => {
                store::cancel(&MutationDb::new(txn.clone()), id, txn.now(), reason).await?;
                Ok(())
            }
            Target::Action { app, identity } => {
                app.typed_mutation::<super::cancel>((id, Some(reason.to_string())).into(), identity).await?;
                Ok(())
            }
        }
    }

    /// Cancel every pending job and running action of workpool `name`, like [`Scheduler::cancel`] (their
    /// `on_complete` mutations run with `canceled`). Returns how many were canceled. In one transaction.
    pub async fn cancel_pool(&self, name: &str) -> crate::Result<u64> {
        let reason = format!("Canceled by {} (with the rest of pool `{name}`)", self.origin);
        match &self.target {
            Target::Mutation(txn) => store::cancel_pool(&MutationDb::new(txn.clone()), name, txn.now(), &reason).await,
            Target::Action { app, identity } => {
                Ok(app.typed_mutation::<super::cancel_pool>((name.to_string(), reason).into(), identity).await?.0)
            }
        }
    }

    async fn schedule(&self, path: String, args: Value, when: When, with: Options) -> crate::Result<Id<ScheduledJob>> {
        let kind = store::kind_of(&path)?;
        let pool = match &with.pool {
            Some(name) => Some(self.pools.get(name).ok_or_else(|| {
                let declare = format!("RxFerrix::workpool(Workpool::new(\"{name}\"))");
                anyhow::anyhow!("no workpool is named `{name}`: declare it with `{declare}`")
            })?),
            None => None,
        };
        let retries = with.retries.or(pool.map(|p| p.retries)).unwrap_or(0);
        let max_attempts = if kind == JobKind::Action { retries.saturating_add(1) } else { 1 };
        let backoff = with.backoff.or(pool.and_then(|p| p.backoff));
        let on_complete = match with.on_complete {
            Some(on_complete) => {
                let on_complete = on_complete.map_err(|e| anyhow::anyhow!("on_complete context: {e}"))?;
                if store::kind_of(&on_complete.path)? != JobKind::Mutation {
                    let path = &on_complete.path;
                    return Err(anyhow::anyhow!("on_complete must be a mutation, and `{path}` is an action"));
                }
                Some(on_complete)
            }
            None => None,
        };
        let origin = Some(self.origin.clone());
        match &self.target {
            Target::Mutation(txn) => {
                let now = txn.now();
                let run_at = when.resolve(now);
                let pool = with.pool;
                let job = NewJob { path, kind, args, run_at, max_attempts, origin, pool, on_complete, backoff };
                store::insert(&MutationDb::new(txn.clone()), job, now).await
            }
            Target::Action { app, identity } => {
                let run_at = when.resolve(app.db().clock().now_ms());
                let args = (path, args, run_at, max_attempts, origin, with.pool, on_complete, backoff);
                Ok(app.typed_mutation::<super::schedule>(args.into(), identity).await?.0)
            }
        }
    }
}

struct Options {
    retries: Option<u32>,
    pool: Option<String>,
    backoff: Option<Backoff>,
    on_complete: Option<Result<OnComplete, String>>,
}

/// [`Scheduler::run_after`] / [`Scheduler::run_at`] with options: from [`Scheduler::with_retries`],
/// [`Scheduler::pool`] or [`Scheduler::on_complete`], which combine.
pub struct ScheduleWith<'a> {
    scheduler: &'a Scheduler,
    retries: Option<u32>,
    pool: Option<String>,
    backoff: Option<Backoff>,
    on_complete: Option<Result<OnComplete, String>>,
}

impl<'a> ScheduleWith<'a> {
    /// See [`Scheduler::with_retries`]. Overrides the pool's default.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = Some(retries);
        self
    }

    /// See [`Scheduler::pool`].
    pub fn pool(mut self, name: &str) -> Self {
        self.pool = Some(name.to_string());
        self
    }

    /// Wait `initial` before the first retry, then `base` times longer before each further one (at most an hour).
    /// Overrides the pool's and the scheduler's backoff.
    pub fn backoff(mut self, initial: Duration, base: f64) -> Self {
        self.backoff = Some(Backoff { initial_ms: initial.as_secs_f64() * 1000.0, base });
        self
    }

    /// Once the job has finished, run mutation `F` with args `{ workId, context, result }`: the job's id, `context`
    /// (serialized now), and how it ended ([`crate::scheduler::WorkResult`]: its return value, its last error once
    /// no retries are left, or `canceled`). It is scheduled in the transaction that records the outcome, so it runs
    /// exactly once (unless the outcome is lost to a restart while an action ran: then it runs with `failed`).
    ///
    /// ```ignore
    /// #[internal_mutation]
    /// pub async fn sent(ctx: &MutationCtx, work_id: Id<ScheduledJob>, context: Id<User>, result: WorkResult<String>)
    ///     -> Result<()> { … }
    /// ```
    pub fn on_complete<F: MutationFunction>(mut self, context: impl serde::Serialize) -> Self {
        let context = serde_json::to_value(context).map_err(|e| e.to_string());
        self.on_complete = Some(context.map(|context| OnComplete { path: F::path(), context }));
        self
    }

    pub async fn run_after<F: SchedulableFunction>(
        self,
        delay: Duration,
        args: impl Into<F::Args>,
    ) -> crate::Result<Id<ScheduledJob>> {
        let args = serde_json::to_value(args.into())?;
        let (scheduler, options) = self.split();
        scheduler.schedule(F::path(), args, When::After(delay), options).await
    }

    pub async fn run_at<F: SchedulableFunction>(
        self,
        at_ms: f64,
        args: impl Into<F::Args>,
    ) -> crate::Result<Id<ScheduledJob>> {
        let args = serde_json::to_value(args.into())?;
        let (scheduler, options) = self.split();
        scheduler.schedule(F::path(), args, When::At(at_ms), options).await
    }

    /// Run `F` with `args` as soon as a slot of its pool is free: `run_after(Duration::ZERO, args)`.
    pub async fn enqueue<F: SchedulableFunction>(self, args: impl Into<F::Args>) -> crate::Result<Id<ScheduledJob>> {
        self.run_after::<F>(Duration::ZERO, args).await
    }

    /// Schedule the function at `path` (checked now) with JSON `args`, after `delay`.
    pub(crate) async fn run_path(self, path: String, args: Value, delay: Duration) -> crate::Result<Id<ScheduledJob>> {
        let (scheduler, options) = self.split();
        scheduler.schedule(path, args, When::After(delay), options).await
    }

    fn split(self) -> (&'a Scheduler, Options) {
        let ScheduleWith { scheduler, retries, pool, backoff, on_complete } = self;
        (scheduler, Options { retries, pool, backoff, on_complete })
    }
}
