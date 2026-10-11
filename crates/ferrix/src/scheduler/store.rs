//! Reads and writes of `_scheduled_jobs` shared by `ctx.scheduler`, the runner, crons and the dashboard.

use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tables::{Backoff, JobKind, JobState, OnComplete, ScheduledJob};
use crate::UserError;
use crate::db::query::{MutationDb, QueryDb, Source};
use crate::id::Id;
use crate::private::FunctionKind;
use crate::runtime::App;

/// Results and errors stored on a job are cut to this many bytes.
pub const MAX_OUTCOME_BYTES: usize = 4096;
/// Serialized args of a scheduled job may be at most this large.
pub const MAX_ARGS_BYTES: usize = 1 << 20;

/// A job to insert.
pub(crate) struct NewJob {
    pub path: String,
    pub kind: JobKind,
    pub args: Value,
    pub run_at: f64,
    pub max_attempts: u32,
    pub origin: Option<String>,
    pub pool: Option<String>,
    pub on_complete: Option<OnComplete>,
    pub backoff: Option<Backoff>,
}

impl NewJob {
    /// A job with no pool, completion callback or custom backoff.
    pub fn new(path: String, kind: JobKind, args: Value, run_at: f64, attempts: u32, origin: Option<String>) -> NewJob {
        let max_attempts = attempts;
        NewJob { path, kind, args, run_at, max_attempts, origin, pool: None, on_complete: None, backoff: None }
    }
}

/// The kind of the function at `path`, if it can be scheduled.
pub(crate) fn kind_of(path: &str) -> crate::Result<JobKind> {
    let def = crate::runtime::registry()?
        .get(path)
        .ok_or_else(|| UserError::new("not_found", format!("No function is registered at `{path}`")))?;
    match def.handler.kind() {
        FunctionKind::Mutation => Ok(JobKind::Mutation),
        FunctionKind::Action => Ok(JobKind::Action),
        FunctionKind::Query => Err(UserError::new(
            "not_schedulable",
            format!("`{path}` is a query; only mutations and actions can be scheduled"),
        )
        .into()),
    }
}

/// Insert a pending job, in the caller's transaction.
pub(crate) async fn insert(db: &MutationDb, job: NewJob, now: f64) -> crate::Result<Id<ScheduledJob>> {
    anyhow::ensure!(job.run_at.is_finite(), "the time to run `{}` at is not a finite number", job.path);
    let size = serde_json::to_string(&job.args)?.len();
    anyhow::ensure!(
        size <= MAX_ARGS_BYTES,
        "args for `{}` are too large to schedule ({size} bytes, max {MAX_ARGS_BYTES})",
        job.path
    );
    let mut value = ScheduledJob {
        path: job.path,
        kind: job.kind,
        args: job.args,
        run_at: job.run_at,
        state: JobState::Pending,
        attempts: 0,
        max_attempts: job.max_attempts.max(1),
        result: None,
        error: None,
        scheduled_at: now,
        started_at: None,
        completed_at: None,
        origin: job.origin,
        pool: job.pool,
        on_complete: job.on_complete,
        backoff: job.backoff,
    };
    let id = db.insert(value.clone()).await?;
    // `scheduledAt` := the document's `_creationTime`, which is strictly increasing across all inserts (even within
    // one transaction, where `now` is fixed). As the last field of the `(state, runAt, scheduledAt)` index it makes
    // jobs due at the same time run in scheduling order (index keys otherwise end in the random `_id`).
    let created = db.get(id).await?.map(|d| d.creation_time).unwrap_or(now);
    if created != now {
        value.scheduled_at = created;
        db.replace(id, value).await?;
    }
    Ok(id)
}

/// How a job ended: the `result` passed to its `on_complete` mutation (`{ workId, context, result }`), as
/// `{ "kind": "success", "returnValue": … }`, `{ "kind": "failed", "error": "…" }` or `{ "kind": "canceled" }`.
///
/// ```ignore
/// #[internal_mutation]
/// pub async fn email_sent(ctx: &MutationCtx, work_id: Id<ScheduledJob>, context: Id<User>, result: WorkResult<String>)
///     -> Result<()> { … }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase", tag = "kind")]
pub enum WorkResult<T = Value> {
    Success {
        #[serde(rename = "returnValue")]
        #[ts(rename = "returnValue")]
        return_value: T,
    },
    Failed {
        error: String,
    },
    Canceled,
}

impl<T> WorkResult<T> {
    /// The return value, or an error for a failed or canceled job.
    pub fn into_result(self) -> crate::Result<T> {
        match self {
            WorkResult::Success { return_value } => Ok(return_value),
            WorkResult::Failed { error } => Err(anyhow::anyhow!("{error}")),
            WorkResult::Canceled => Err(anyhow::anyhow!("canceled")),
        }
    }
}

/// A job has finished (`job` is its final state, about to be written in the caller's transaction): if it has an
/// `on_complete`, schedule that mutation, due now, in the same transaction, so it runs exactly once, after the
/// outcome is committed. A return value too large to pass on (over [`MAX_ARGS_BYTES`]) is passed as a failure.
pub(crate) async fn finished(
    db: &MutationDb,
    id: Id<ScheduledJob>,
    job: &ScheduledJob,
    result: WorkResult,
    now: f64,
) -> crate::Result<()> {
    let Some(on_complete) = &job.on_complete else { return Ok(()) };
    let origin = format!("onComplete:{id}");
    schedule_on_complete(db, on_complete, ("workId", serde_json::to_value(id)?), result, now, origin).await
}

/// Schedule `on_complete`, due now, with args `{ <key>: <id>, context, result }`. A return value too large to pass on
/// (over [`MAX_ARGS_BYTES`]) is passed as a failure.
pub(crate) async fn schedule_on_complete(
    db: &MutationDb,
    on_complete: &OnComplete,
    (key, id): (&str, Value),
    result: WorkResult,
    now: f64,
    origin: String,
) -> crate::Result<()> {
    let mut args = serde_json::json!({ key: id, "context": on_complete.context, "result": result });
    let size = serde_json::to_string(&args)?.len();
    if size > MAX_ARGS_BYTES {
        let error = format!("the return value is too large to pass to onComplete ({size} bytes, max {MAX_ARGS_BYTES})");
        args["result"] = serde_json::to_value(WorkResult::<Value>::Failed { error })?;
    }
    insert(db, NewJob::new(on_complete.path.clone(), JobKind::Mutation, args, now, 1, Some(origin)), now).await?;
    Ok(())
}

/// What [`cancel`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum CancelOutcome {
    /// It was pending, or an action in progress: now it is canceled (a running action is signaled once the cancel
    /// commits, and aborted after `SchedulerOptions::cancel_grace` if it doesn't stop by itself).
    Canceled,
    /// It had already finished (success, failed or canceled): nothing changed.
    AlreadyFinished,
}

/// Cancel a job, in the caller's transaction, recording `reason` in its `error`. `not_found` (a user error) for an
/// unknown id. Canceling an action in progress also signals it once the transaction commits (`super::cancellation`).
pub(crate) async fn cancel(
    db: &MutationDb,
    id: Id<ScheduledJob>,
    now: f64,
    reason: &str,
) -> crate::Result<CancelOutcome> {
    let Some(job) = db.get(id).await? else {
        return Err(UserError::new("not_found", format!("Scheduled job {id} does not exist")).into());
    };
    let running = match job.state {
        JobState::Pending => false,
        JobState::InProgress => true,
        _ => return Ok(CancelOutcome::AlreadyFinished),
    };
    let mut value = job.value;
    value.state = JobState::Canceled;
    value.completed_at = Some(now);
    let reason = truncate(if running { format!("{reason} (while running)") } else { reason.to_string() });
    value.error = Some(reason.clone());
    finished(db, id, &value, WorkResult::Canceled, now).await?;
    db.replace(id, value).await?;
    if running {
        db.txn().on_commit(move || super::cancellation::signal(id, &reason));
    }
    Ok(CancelOutcome::Canceled)
}

/// Cancel every pending job and running action of workpool `pool`, in the caller's transaction. Returns how many.
pub(crate) async fn cancel_pool(db: &MutationDb, pool: &str, now: f64, reason: &str) -> crate::Result<u64> {
    let mut canceled = 0;
    for kind in [JobKind::Mutation, JobKind::Action] {
        for state in [JobState::Pending, JobState::InProgress] {
            let jobs = db
                .query::<ScheduledJob>()
                .with_index(ScheduledJob::by_kind_pool_state_run_at, |q| {
                    q.eq(ScheduledJob::kind, kind)
                        .eq(ScheduledJob::pool, Some(pool.to_string()))
                        .eq(ScheduledJob::state, state)
                })
                .collect()
                .await?;
            for job in jobs {
                if cancel(db, job.id, now, reason).await? == CancelOutcome::Canceled {
                    canceled += 1;
                }
            }
        }
    }
    Ok(canceled)
}

/// `s` cut to [`MAX_OUTCOME_BYTES`] (on a char boundary, with `…` appended when cut).
pub(crate) fn truncate(mut s: String) -> String {
    if s.len() <= MAX_OUTCOME_BYTES {
        return s;
    }
    let mut cut = MAX_OUTCOME_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push('…');
    s
}

/// Run `f` against a fresh snapshot.
pub(crate) async fn read<R, F, Fut>(app: &App, f: F) -> crate::Result<R>
where
    F: FnOnce(QueryDb) -> Fut,
    Fut: Future<Output = crate::Result<R>>,
{
    let snapshot = app.db().snapshot().await?;
    f(QueryDb::new(Source::Snapshot(snapshot), app.db().catalog().clone())).await
}

/// Run `f` in one write transaction (not a registered function: no execution log entry); commit when it returns
/// `Ok`.
pub(crate) async fn write<R, F, Fut>(app: &App, f: F) -> crate::Result<R>
where
    F: FnOnce(MutationDb) -> Fut,
    Fut: Future<Output = crate::Result<R>>,
{
    let txn = app.db().begin_write().await?;
    match f(MutationDb::new(txn.clone())).await {
        Ok(v) => {
            txn.commit_with(|_, _| Ok(())).await?;
            Ok(v)
        }
        Err(e) => {
            txn.rollback();
            Err(e)
        }
    }
}
