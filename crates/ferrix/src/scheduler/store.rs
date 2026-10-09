//! Reads and writes of `_scheduled_jobs` shared by `ctx.scheduler`, the runner, crons and the dashboard.

use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tables::{JobKind, JobState, ScheduledJob};
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
}

/// The kind of the function at `path`, if it can be scheduled.
pub(crate) fn kind_of(path: &str) -> crate::Result<JobKind> {
    let def = crate::runtime::registry()?
        .get(path)
        .ok_or_else(|| UserError::new("not_found", format!("No function is registered at `{path}`")))?;
    match def.handler.kind() {
        FunctionKind::Mutation => Ok(JobKind::Mutation),
        FunctionKind::Action => Ok(JobKind::Action),
        FunctionKind::Query => {
            Err(UserError::new("not_schedulable", format!("`{path}` is a query; only mutations and actions can be scheduled")).into())
        }
    }
}

/// Insert a pending job, in the caller's transaction.
pub(crate) async fn insert(db: &MutationDb, job: NewJob, now: f64) -> crate::Result<Id<ScheduledJob>> {
    anyhow::ensure!(job.run_at.is_finite(), "the time to run `{}` at is not a finite number", job.path);
    let size = serde_json::to_string(&job.args)?.len();
    anyhow::ensure!(size <= MAX_ARGS_BYTES, "args for `{}` are too large to schedule ({size} bytes, max {MAX_ARGS_BYTES})", job.path);
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
pub(crate) async fn cancel(db: &MutationDb, id: Id<ScheduledJob>, now: f64, reason: &str) -> crate::Result<CancelOutcome> {
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
    db.replace(id, value).await?;
    if running {
        db.txn().on_commit(move || super::cancellation::signal(id, &reason));
    }
    Ok(CancelOutcome::Canceled)
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
            txn.commit_with(|_, _| Ok(()))?;
            Ok(v)
        }
        Err(e) => {
            txn.rollback();
            Err(e)
        }
    }
}
