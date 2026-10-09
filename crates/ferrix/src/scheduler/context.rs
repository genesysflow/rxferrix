//! `ctx.scheduler`: schedule mutations and actions from mutations (transactionally) and from actions (immediately).

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::store::{self, NewJob};
use super::tables::{JobKind, ScheduledJob};
use crate::db::WriteTxn;
use crate::db::query::MutationDb;
use crate::id::Id;
use crate::runtime::{App, Identity};
use crate::SchedulableFunction;

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
/// not compile. Args are serialized when the job is scheduled. Scheduled functions run with no user identity
/// (`ctx.auth` is anonymous; the execution log shows the caller as `scheduler`). Mutations run exactly once; actions
/// at most once, see [`crate::scheduler`].
pub struct Scheduler {
    target: Target,
    /// The scheduling function's path, recorded as the job's `origin`.
    origin: String,
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
    pub(crate) fn in_mutation(txn: Arc<WriteTxn>, origin: &str) -> Scheduler {
        Scheduler { target: Target::Mutation(txn), origin: origin.to_string() }
    }

    pub(crate) fn in_action(app: App, identity: Identity, origin: &str) -> Scheduler {
        Scheduler { target: Target::Action { app, identity }, origin: origin.to_string() }
    }

    /// Run `F` with `args` once `delay` has passed (from this mutation's `now()`, or from now in an action).
    /// Returns the job's id (for [`Scheduler::cancel`]).
    pub async fn run_after<F: SchedulableFunction>(
        &self,
        delay: Duration,
        args: impl Into<F::Args>,
    ) -> crate::Result<Id<ScheduledJob>> {
        self.with_retries(0).run_after::<F>(delay, args).await
    }

    /// Run `F` with `args` at `at_ms` (ms since the epoch; a time in the past means "as soon as possible").
    pub async fn run_at<F: SchedulableFunction>(&self, at_ms: f64, args: impl Into<F::Args>) -> crate::Result<Id<ScheduledJob>> {
        self.with_retries(0).run_at::<F>(at_ms, args).await
    }

    /// Opt into retries for an **action**: if it returns an error, it is run again up to `retries` more times, with
    /// exponential backoff (`SchedulerOptions::retry_backoff`, doubled each time). Ignored for mutations (a failing
    /// mutation fails the same way again). An action interrupted by a restart is never retried: it may have had
    /// side effects.
    ///
    /// `ctx.scheduler.with_retries(3).run_after::<send_email>(Duration::ZERO, (to,)).await?`
    pub fn with_retries(&self, retries: u32) -> ScheduleWith<'_> {
        ScheduleWith { scheduler: self, retries }
    }

    /// Cancel a job. A pending job becomes `canceled` and won't run; canceling a finished job does nothing, and an
    /// action already in progress runs to completion. Fails with the user error `not_found` for an unknown id.
    /// Inside a mutation, the cancel commits (or rolls back) with it.
    pub async fn cancel(&self, id: Id<ScheduledJob>) -> crate::Result<()> {
        match &self.target {
            Target::Mutation(txn) => {
                store::cancel(&MutationDb::new(txn.clone()), id, txn.now()).await?;
                Ok(())
            }
            Target::Action { app, identity } => {
                app.typed_mutation::<super::cancel>((id,).into(), identity).await?;
                Ok(())
            }
        }
    }

    async fn schedule(&self, path: String, args: Value, when: When, retries: u32) -> crate::Result<Id<ScheduledJob>> {
        let kind = store::kind_of(&path)?;
        let max_attempts = if kind == JobKind::Action { retries.saturating_add(1) } else { 1 };
        match &self.target {
            Target::Mutation(txn) => {
                let now = txn.now();
                let job =
                    NewJob { path, kind, args, run_at: when.resolve(now), max_attempts, origin: Some(self.origin.clone()) };
                store::insert(&MutationDb::new(txn.clone()), job, now).await
            }
            Target::Action { app, identity } => {
                let run_at = when.resolve(app.db().clock().now_ms());
                let args = (path, args, run_at, max_attempts, Some(self.origin.clone()));
                Ok(app.typed_mutation::<super::schedule>(args.into(), identity).await?.0)
            }
        }
    }
}

/// [`Scheduler::run_after`] / [`Scheduler::run_at`] with options, from [`Scheduler::with_retries`].
pub struct ScheduleWith<'a> {
    scheduler: &'a Scheduler,
    retries: u32,
}

impl ScheduleWith<'_> {
    pub async fn run_after<F: SchedulableFunction>(
        self,
        delay: Duration,
        args: impl Into<F::Args>,
    ) -> crate::Result<Id<ScheduledJob>> {
        let args = serde_json::to_value(args.into())?;
        self.scheduler.schedule(F::path(), args, When::After(delay), self.retries).await
    }

    pub async fn run_at<F: SchedulableFunction>(self, at_ms: f64, args: impl Into<F::Args>) -> crate::Result<Id<ScheduledJob>> {
        let args = serde_json::to_value(args.into())?;
        self.scheduler.schedule(F::path(), args, When::At(at_ms), self.retries).await
    }
}
