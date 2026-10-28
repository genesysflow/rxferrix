//! The job runner: finds due jobs and runs them with bounded concurrency.
//!
//! One dispatcher at a time (the background loop, or `App::run_due_jobs` in tests) reads the due jobs in
//! `(runAt, scheduling order)` order through the `(state, runAt)` index and starts them in that order:
//!
//! - **Mutation jobs run one after another, inside the dispatcher.** They would serialize on the single writer
//!   anyway, and this keeps their order deterministic. The job's row is checked (still `pending`) and updated to
//!   `success` *inside the mutation's own transaction* ([`JobTxn`]), so the job's writes and its completion commit
//!   together: **exactly once**. If the mutation fails, its transaction is rolled back and a separate transaction
//!   marks the job `failed`. A crash before the commit leaves the job `pending` with none of its writes, so it simply
//!   runs (once) after the restart.
//! - **Action jobs** are marked `inProgress` in a committed transaction first, then run in a spawned task; the
//!   outcome is recorded afterwards. Actions can do anything (network I/O), so a crash mid-run can't be undone: at
//!   startup, jobs still `inProgress` become `failed` ("server restarted while running"). **At most once.**
//!   Actions scheduled `with_retries(n)` go back to `pending` (with backoff) after an error.
//!
//! Every running action holds a slot of its pool: the default pool's `SchedulerOptions::concurrency`, or its
//! workpool's `max_parallelism` (`super::Workpool`). Each pool's due actions start in order while it has free slots;
//! a full pool doesn't hold up the others. Mutation jobs need no slot (they run one at a time anyway) and run first
//! in each round, through the `(kind, state, runAt)` index; actions through `(kind, pool, state, runAt)`.
//!
//! The transaction that records a job's final state (success, failed for good, canceled) also schedules its
//! `on_complete` mutation, if any (`store::finished`).
//!
//! The background loop sleeps until the next job or cron is due, and wakes early when a commit writes to
//! `_scheduled_jobs` (it watches the write log, so jobs scheduled by any mutation, action, or the dashboard are
//! picked up without any explicit hook), or when poked through [`SchedulerState::notify`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use super::cancellation::{self as cancel, ActionAborted, Registration};
use super::cron::{self, ParsedCron};
use std::collections::BTreeMap;

use super::store::{self, WorkResult};
use super::tables::{Cron, JobKind, JobState, ScheduledJob};
use super::{IN_JOB, SchedulerOptions, Workpool};
use crate::Table;
use crate::db::Clock;
use crate::db::query::MutationDb;
use crate::guard::Origin;
use crate::id::Id;
use crate::private::FunctionKind;
use crate::runtime::{App, Identity, MutationHook};
use crate::schema::Doc;

/// Due jobs read per round.
const BATCH: usize = 128;
/// Finished jobs deleted (or moved to the job history) per transaction by the cleanup.
const CLEANUP_BATCH: usize = 256;
/// The background loop re-checks at least this often (a safety net; it normally wakes exactly when needed).
const MAX_SLEEP: Duration = Duration::from_secs(60);
/// Retry backoff never grows beyond this.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// The error recorded for actions interrupted by a restart.
pub const INTERRUPTED: &str = "server restarted while running";

/// A pool's slots: one per action it may run at once.
struct Slots {
    semaphore: Arc<Semaphore>,
    size: u32,
}

impl Slots {
    fn new(size: usize) -> Arc<Slots> {
        let size = size.clamp(1, Semaphore::MAX_PERMITS) as u32;
        Arc::new(Slots { semaphore: Arc::new(Semaphore::new(size as usize)), size })
    }
}

/// The scheduler's in-memory state, one per `App`.
pub struct SchedulerState {
    options: SchedulerOptions,
    /// The declared crons, then those set at runtime (`App::set_runtime_cron`).
    crons: std::sync::RwLock<Arc<Vec<ParsedCron>>>,
    /// Names of the crons declared at startup (`SchedulerOptions::crons` and the system crons): not changeable.
    declared: std::collections::BTreeSet<String>,
    /// The declared workpools, by name.
    pools: Arc<BTreeMap<String, Workpool>>,
    /// Slots of the default pool (`None`), the declared pools, and pools that pending jobs name but the app no longer
    /// declares (found at startup; default size).
    slots: std::sync::Mutex<BTreeMap<Option<String>, Arc<Slots>>>,
    notify: Notify,
    init: tokio::sync::OnceCell<()>,
    started: AtomicBool,
    dispatch: tokio::sync::Mutex<()>,
    last_cleanup: std::sync::Mutex<Option<f64>>,
}

impl SchedulerState {
    /// Validates the declared crons (fails `App::open` on a bad expression or duplicate name) and adds rxferrix's
    /// system crons (`cron::system_crons`).
    pub(crate) fn new(app_options: &crate::runtime::AppOptions) -> crate::Result<SchedulerState> {
        let options = &app_options.scheduler;
        let mut specs = options.crons.clone();
        specs.extend(cron::system_crons(app_options));
        let crons = cron::parse_all(&specs)?;
        let declared = crons.iter().map(|c| c.name.clone()).collect();
        let mut pools = BTreeMap::new();
        let mut slots = BTreeMap::from([(None, Slots::new(options.concurrency))]);
        for pool in &options.pools {
            let name = &pool.name;
            let length_ok = !name.is_empty() && name.len() <= 64;
            anyhow::ensure!(length_ok, "workpool names are 1 to 64 characters long: `{name}`");
            anyhow::ensure!(pool.max_parallelism >= 1, "workpool `{name}` must allow at least 1 action at a time");
            if let Some(b) = pool.backoff {
                anyhow::ensure!(b.initial_ms >= 0.0 && b.base >= 1.0, "workpool `{name}`: the backoff must not shrink");
            }
            anyhow::ensure!(pools.insert(name.clone(), pool.clone()).is_none(), "two workpools are named `{name}`");
            slots.insert(Some(name.clone()), Slots::new(pool.max_parallelism));
        }
        Ok(SchedulerState {
            options: options.clone(),
            crons: std::sync::RwLock::new(Arc::new(crons)),
            declared,
            pools: Arc::new(pools),
            slots: std::sync::Mutex::new(slots),
            notify: Notify::new(),
            init: tokio::sync::OnceCell::new(),
            started: AtomicBool::new(false),
            dispatch: tokio::sync::Mutex::new(()),
            last_cleanup: std::sync::Mutex::new(None),
        })
    }

    /// The crons in effect now.
    pub(crate) fn crons(&self) -> Arc<Vec<ParsedCron>> {
        self.crons.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether `name` is declared at startup (by the app, its options or rxferrix), so not settable at runtime.
    pub(crate) fn is_declared(&self, name: &str) -> bool {
        self.declared.contains(name)
    }

    /// Wake the background loop (it re-reads what is due).
    pub(crate) fn notify(&self) {
        self.notify.notify_one();
    }

    /// The declared workpools.
    pub(crate) fn pools(&self) -> &Arc<BTreeMap<String, Workpool>> {
        &self.pools
    }

    fn all_slots(&self) -> Vec<(Option<String>, Arc<Slots>)> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn slots_of(&self, pool: &Option<String>) -> Arc<Slots> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.entry(pool.clone()).or_insert_with(|| Slots::new(self.options.concurrency)).clone()
    }
}

impl App {
    /// Start the scheduler: recover jobs interrupted by a restart, reconcile `_crons`, then run due jobs and crons
    /// in a background task for as long as the process lives. `rxferrix serve` calls this; calling it again does
    /// nothing. (Tests usually drive the scheduler by hand instead: `TestApp::run_due_jobs`, `advance_time`.)
    pub async fn start_scheduler(&self) -> crate::Result<()> {
        self.init_scheduler().await?;
        if !self.scheduler_state().started.swap(true, Ordering::SeqCst) {
            tokio::spawn(run_loop(self.clone()));
        }
        Ok(())
    }

    /// Wait up to `grace` for the scheduled actions still running to finish (the server is stopping: nothing new
    /// starts once it drains). One still running then is left `inProgress`; the next start retries it if it has
    /// attempts left (`recover_interrupted`).
    pub(crate) async fn finish_running_actions(&self, grace: Duration) {
        let st = self.scheduler_state();
        let pools = st.all_slots();
        let all = async {
            // Holding every slot of every pool means every running action has finished.
            let mut held = Vec::new();
            for (_, slots) in &pools {
                held.extend(slots.semaphore.acquire_many(slots.size).await.ok());
            }
        };
        if tokio::time::timeout(grace, all).await.is_err() {
            tracing::warn!(
                "scheduled actions still running {grace:?} after shutdown; they are retried at the next start"
            );
        }
    }

    /// Run every job (and cron) that is due now, wait for them to finish, and repeat until nothing is due (jobs may
    /// schedule more jobs). Returns how many jobs ran. Initializes the scheduler first if needed.
    pub async fn run_due_jobs(&self) -> crate::Result<usize> {
        self.init_scheduler().await?;
        let st = self.scheduler_state();
        let mut total = 0;
        loop {
            let ran = dispatch(self).await?;
            // Holding every slot means every running action has finished.
            for (_, slots) in st.all_slots() {
                drop(slots.semaphore.acquire_many(slots.size).await?);
            }
            if ran == 0 {
                return Ok(total);
            }
            total += ran;
        }
    }

    /// Startup work, once per `App`: jobs left `inProgress` by a previous process become `failed`, `_crons` is
    /// reconciled with the declared crons, and old finished jobs are cleaned up.
    pub(crate) async fn init_scheduler(&self) -> crate::Result<()> {
        self.scheduler_state()
            .init
            .get_or_try_init(|| async {
                let now = self.db().clock().now_ms();
                let recovered = recover_interrupted(self, now).await?;
                if recovered > 0 {
                    tracing::warn!("{recovered} scheduled action(s) were interrupted by a restart and marked failed");
                }
                discover_pools(self).await?;
                cron::reconcile(self, &self.scheduler_state().crons(), now).await?;
                maybe_cleanup(self, now).await?;
                Ok::<_, crate::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Declare (`Some`) or withdraw (`None`) the cron `name` while the app runs (settings-driven crons such as the
    /// backup schedule). Crons declared at startup can't be replaced. `_crons` is updated now if the scheduler has
    /// started (else at its startup reconcile), and the loop wakes up to pick up the new `nextRun`.
    pub(crate) async fn set_runtime_cron(
        &self,
        name: &str,
        spec: Option<crate::scheduler::CronSpec>,
    ) -> crate::Result<()> {
        let st = self.scheduler_state();
        anyhow::ensure!(
            !st.is_declared(name),
            "the cron `{name}` is declared at startup and can't be changed at runtime"
        );
        let parsed = match spec {
            Some(spec) => {
                anyhow::ensure!(spec.name() == name, "cron name mismatch: `{}` set as `{name}`", spec.name());
                cron::parse_all(std::slice::from_ref(&spec))?.pop()
            }
            None => None,
        };
        {
            let mut crons = st.crons.write().unwrap_or_else(|e| e.into_inner());
            let mut list: Vec<ParsedCron> = crons.iter().filter(|c| c.name != name).cloned().collect();
            list.extend(parsed.clone());
            *crons = Arc::new(list);
        }
        if st.init.initialized() {
            cron::reconcile_one(self, name, parsed.as_ref(), self.db().clock().now_ms()).await?;
            st.notify();
        }
        Ok(())
    }

    /// Run the scheduler's cleanup now (it also runs periodically): move finished jobs older than
    /// `SchedulerOptions::history_after` to the job history, and delete finished jobs and workflows older than the
    /// retention period. Returns how many jobs were moved or deleted from `_scheduled_jobs`.
    pub async fn cleanup_scheduled_jobs(&self) -> crate::Result<usize> {
        let now = self.db().clock().now_ms();
        cleanup(self, now).await
    }
}

/// Jobs left `inProgress` (actions that were running when the process stopped) are retried if they have attempts
/// left (scheduled `with_retries`, so they are meant to run again after a failure), and become `failed` otherwise.
async fn recover_interrupted(app: &App, now: f64) -> crate::Result<usize> {
    let backoff_base = app.scheduler_state().options.retry_backoff;
    store::write(app, |db| async move {
        let running = db
            .query::<ScheduledJob>()
            .with_index(ScheduledJob::by_state_run_at, |q| q.eq(ScheduledJob::state, JobState::InProgress))
            .collect()
            .await?;
        for job in &running {
            let mut value = job.value.clone();
            value.error = Some(INTERRUPTED.to_string());
            if value.attempts < value.max_attempts {
                value.state = JobState::Pending;
                value.run_at = now + retry_delay(&value, backoff_base).as_secs_f64() * 1000.0;
            } else {
                value.state = JobState::Failed;
                value.completed_at = Some(now);
                store::finished(&db, job.id, &value, WorkResult::Failed { error: INTERRUPTED.to_string() }, now)
                    .await?;
            }
            db.replace(job.id, value).await?;
        }
        Ok(running.len())
    })
    .await
}

/// Give pools that jobs name but the app no longer declares slots of their own (default size), so their pending
/// actions still run. Walks the distinct pool names of action jobs through the `(kind, pool, …)` index.
async fn discover_pools(app: &App) -> crate::Result<()> {
    let st = app.scheduler_state();
    let mut after: Option<String> = None;
    loop {
        let last = after.clone();
        let next = store::read(app, |db| async move {
            db.query::<ScheduledJob>()
                .with_index(ScheduledJob::by_kind_pool_state_run_at, |q| {
                    q.eq(ScheduledJob::kind, JobKind::Action).gt(ScheduledJob::pool, last)
                })
                .first()
                .await
        })
        .await?;
        let Some(name) = next.and_then(|job| job.value.pool) else { return Ok(()) };
        if !st.pools.contains_key(&name) {
            let size = st.options.concurrency;
            tracing::warn!(pool = %name, "scheduled actions name a workpool that is no longer declared ({size} at a time)");
            st.slots_of(&Some(name.clone()));
        }
        after = Some(name);
    }
}

/// Run the retention cleanup if it hasn't run within `cleanup_interval`.
async fn maybe_cleanup(app: &App, now: f64) -> crate::Result<()> {
    let st = app.scheduler_state();
    let interval_ms = st.options.cleanup_interval.as_secs_f64() * 1000.0;
    {
        let mut last = st.last_cleanup.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| now - t < interval_ms) {
            return Ok(());
        }
        *last = Some(now);
    }
    cleanup(app, now).await?;
    Ok(())
}

/// Delete success, failed and canceled jobs that finished more than `retention` ago (and finished workflows, with
/// their steps), move those that finished more than `history_after` ago to the job history, and delete history rows
/// past the retention. Returns how many jobs left `_scheduled_jobs`.
async fn cleanup(app: &App, now: f64) -> crate::Result<usize> {
    let options = &app.scheduler_state().options;
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let retention_cutoff = options.retention.map(|r| now - ms(r));
    let mut total = 0;
    if let Some(cutoff) = retention_cutoff {
        let workflows = crate::workflow::cleanup(app, cutoff).await?;
        if workflows > 0 {
            tracing::info!("deleted {workflows} finished workflow(s) past their retention");
        }
        let deleted = delete_finished(app, cutoff).await?;
        if deleted > 0 {
            tracing::info!("deleted {deleted} finished scheduled job(s) past their retention");
        }
        total += deleted;
    }
    if let Some(after) = options.history_after {
        let moved = move_to_history(app, now - ms(after)).await?;
        if moved > 0 {
            tracing::info!("moved {moved} finished scheduled job(s) to the job history");
        }
        total += moved;
    }
    if let Some(cutoff) = retention_cutoff {
        let trimmed = app.logs().write_with(move |conn| super::history::trim(conn, cutoff)).await?;
        if trimmed > 0 {
            tracing::info!("deleted {trimmed} job(s) past their retention from the job history");
        }
    }
    Ok(total)
}

/// Move the jobs that finished before `cutoff` from `_scheduled_jobs` to the job history, a batch at a time: each
/// batch is committed to the history first, then deleted from `_scheduled_jobs` (see [`super::history`]). The last
/// job of every cron stays, so the cron's state is still known. Returns how many.
async fn move_to_history(app: &App, cutoff: f64) -> crate::Result<usize> {
    let mut total = 0;
    loop {
        let (batch, more) = store::read(app, |db| async move {
            let keep: std::collections::BTreeSet<_> =
                db.query::<Cron>().collect().await?.into_iter().filter_map(|c| c.value.last_job).collect();
            // Fetching past the kept jobs guarantees a full batch whenever there is more to move.
            let limit = CLEANUP_BATCH + keep.len();
            let old = db
                .query::<ScheduledJob>()
                .with_index(ScheduledJob::by_completed_at, |q| {
                    q.gt(ScheduledJob::completed_at, None).lt(ScheduledJob::completed_at, Some(cutoff))
                })
                .take(limit)
                .await?;
            let more = old.len() == limit;
            Ok((old.into_iter().filter(|job| !keep.contains(&job.id)).collect::<Vec<_>>(), more))
        })
        .await?;
        if !batch.is_empty() {
            let ids: Vec<Id<ScheduledJob>> = batch.iter().map(|job| job.id).collect();
            total += batch.len();
            app.logs().write_with(move |conn| super::history::insert(conn, &batch)).await?;
            store::write(app, |db| async move {
                for id in ids {
                    if db.get(id).await?.is_some() {
                        db.delete(id).await?;
                    }
                }
                Ok(())
            })
            .await?;
        }
        if !more {
            return Ok(total);
        }
    }
}

/// Delete the jobs that finished before `cutoff` from `_scheduled_jobs`. Returns how many.
async fn delete_finished(app: &App, cutoff: f64) -> crate::Result<usize> {
    let mut total = 0;
    loop {
        let deleted = store::write(app, |db| async move {
            let old = db
                .query::<ScheduledJob>()
                .with_index(ScheduledJob::by_completed_at, |q| {
                    q.gt(ScheduledJob::completed_at, None).lt(ScheduledJob::completed_at, Some(cutoff))
                })
                .take(CLEANUP_BATCH)
                .await?;
            for job in &old {
                db.delete(job.id).await?;
            }
            Ok(old.len())
        })
        .await?;
        total += deleted;
        if deleted < CLEANUP_BATCH {
            return Ok(total);
        }
    }
}

/// One dispatch: rounds of firing due crons, then running the due mutation jobs and starting the due actions of
/// every pool with free slots, until nothing more is due (or every pool with due actions is full). Crons fire (and
/// the cleanup runs) every round, so a long queue of due jobs doesn't hold them back. Returns how many jobs were
/// started. Starts nothing once the server is draining (shutting down or handing over).
async fn dispatch(app: &App) -> crate::Result<usize> {
    let st = app.scheduler_state();
    let _one_dispatcher = st.dispatch.lock().await;
    let clock = app.db().clock();
    let mut started = 0;
    loop {
        if app.is_draining() {
            return Ok(started);
        }
        if let Err(e) = maybe_cleanup(app, clock.now_ms()).await {
            tracing::error!("scheduled job cleanup failed: {e:#}");
        }
        cron::fire_due(app, &st.crons(), clock.now_ms()).await?;
        let mut round = run_due_mutations(app).await?;
        for (pool, slots) in st.all_slots() {
            round += start_due_actions(app, pool, &slots).await?;
        }
        if round == 0 {
            return Ok(started);
        }
        started += round;
    }
}

/// Run due mutation jobs, in order, one at a time (up to a batch). Returns how many ran.
async fn run_due_mutations(app: &App) -> crate::Result<usize> {
    let now = app.db().clock().now_ms();
    let due = store::read(app, |db| async move {
        db.query::<ScheduledJob>()
            .with_index(ScheduledJob::by_kind_state_run_at, |q| {
                q.eq(ScheduledJob::kind, JobKind::Mutation)
                    .eq(ScheduledJob::state, JobState::Pending)
                    .lte(ScheduledJob::run_at, now)
            })
            .take(BATCH)
            .await
    })
    .await?;
    let mut ran = 0;
    for job in due {
        if run_job(app, job, None).await? {
            ran += 1;
        }
    }
    Ok(ran)
}

/// Start due actions of `pool`, in order, while it has free slots. Returns how many started.
async fn start_due_actions(app: &App, pool: Option<String>, slots: &Slots) -> crate::Result<usize> {
    let free = slots.semaphore.available_permits();
    if free == 0 {
        return Ok(0);
    }
    let now = app.db().clock().now_ms();
    let due = store::read(app, |db| async move {
        db.query::<ScheduledJob>()
            .with_index(ScheduledJob::by_kind_pool_state_run_at, |q| {
                q.eq(ScheduledJob::kind, JobKind::Action)
                    .eq(ScheduledJob::pool, pool)
                    .eq(ScheduledJob::state, JobState::Pending)
                    .lte(ScheduledJob::run_at, now)
            })
            .take(free.min(BATCH))
            .await
    })
    .await?;
    let mut started = 0;
    for job in due {
        let Ok(permit) = slots.semaphore.clone().try_acquire_owned() else { break };
        if run_job(app, job, Some(permit)).await? {
            started += 1;
        }
    }
    Ok(started)
}

/// Start one job: a mutation runs now; an action is claimed and spawned, holding `permit` (a slot of its pool;
/// taken here if the job was found as a mutation but is an action now). Returns false if it was no longer pending
/// (someone else ran or canceled it).
async fn run_job(app: &App, job: Doc<ScheduledJob>, permit: Option<OwnedSemaphorePermit>) -> crate::Result<bool> {
    let kind = app.registry().get(&job.path).map(|def| def.handler.kind());
    match kind {
        Some(FunctionKind::Mutation) => {
            let ran = run_mutation_job(app, &job).await;
            drop(permit);
            ran
        }
        Some(FunctionKind::Action) => {
            let permit = match permit {
                Some(p) => p,
                None => app.scheduler_state().slots_of(&job.pool).semaphore.clone().acquire_owned().await?,
            };
            // Registered before the claim commits, so a cancel committed right after it finds the running job.
            let registration = cancel::register(job.id, app.scheduler_state().options.cancel_grace);
            if !claim_action(app, job.id).await? {
                return Ok(false);
            }
            let app = app.clone();
            let signal = registration.signal.clone();
            tokio::spawn(IN_JOB.scope(
                (),
                cancel::scope(signal, async move {
                    run_action_job(&app, job, &registration).await;
                    drop(registration);
                    drop(permit);
                    // A slot is free: its pool's next due action can start.
                    app.scheduler_state().notify();
                }),
            ));
            Ok(true)
        }
        other => {
            let msg = match other {
                Some(FunctionKind::Query) => {
                    format!("`{}` is a query; only mutations and actions can be scheduled", job.path)
                }
                _ => format!("No mutation or action is registered at `{}` any more", job.path),
            };
            fail_pending(app, job.id, &msg).await
        }
    }
}

/// The job's bookkeeping inside its mutation's transaction.
struct JobTxn<'a> {
    id: Id<ScheduledJob>,
    clock: &'a Clock,
}

/// `JobTxn::before` found the job no longer pending: nothing ran.
#[derive(Debug)]
struct NotPending;

impl std::fmt::Display for NotPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the scheduled job is no longer pending")
    }
}

impl std::error::Error for NotPending {}

impl MutationHook for JobTxn<'_> {
    fn before<'a>(&'a self, db: &'a MutationDb) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async move {
            let job = db.get(self.id).await?;
            match job {
                Some(job) if job.state == JobState::Pending => Ok(()),
                _ => Err(NotPending.into()),
            }
        })
    }

    fn after<'a>(&'a self, db: &'a MutationDb, value: &'a Value) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(async move {
            let Some(job) = db.get(self.id).await? else { return Err(NotPending.into()) };
            let now = self.clock.now_ms();
            let mut job = job.value;
            job.state = JobState::Success;
            job.attempts += 1;
            job.started_at = Some(job.started_at.unwrap_or(now));
            job.completed_at = Some(now);
            job.result = Some(store::truncate(serde_json::to_string(value)?));
            job.error = None;
            store::finished(db, self.id, &job, WorkResult::Success { return_value: value.clone() }, now).await?;
            db.replace(self.id, job).await
        })
    }
}

/// Run a mutation job: its writes and its `success` commit together; a failure is recorded separately.
async fn run_mutation_job(app: &App, job: &Doc<ScheduledJob>) -> crate::Result<bool> {
    let hook = JobTxn { id: job.id, clock: app.db().clock() };
    let identity = Identity::anonymous();
    let started = app.db().clock().now_ms();
    let call = app.call_mutation_hooked(&job.path, job.args.clone(), &identity, Origin::Server, None, Some(&hook));
    let run = IN_JOB.scope((), call).await;
    match run.result {
        Ok(_) => Ok(true),
        Err(e) if e.is::<NotPending>() => Ok(false),
        Err(e) => {
            let error = store::truncate(format!("{e:#}"));
            let failed = error.clone();
            let recorded = store::write(app, |db| async move {
                let Some(current) = db.get(job.id).await? else { return Ok(false) };
                if current.state != JobState::Pending {
                    return Ok(false);
                }
                let mut value = current.value;
                let now = app.db().clock().now_ms();
                value.state = JobState::Failed;
                value.attempts += 1;
                value.started_at = Some(started);
                value.completed_at = Some(now);
                value.error = Some(error.clone());
                store::finished(&db, job.id, &value, WorkResult::Failed { error }, now).await?;
                db.replace(job.id, value).await?;
                Ok(true)
            })
            .await?;
            // In a transaction of its own: if ending the workflow fails too, the job stays failed (logged).
            if recorded && let Err(e) = crate::workflow::engine_job_failed(app, &job.path, &job.args, &failed).await {
                tracing::error!(job = %job.id, "could not end the workflow of a failed engine job: {e:#}");
            }
            Ok(recorded)
        }
    }
}

/// Mark a pending job `failed` without running it.
async fn fail_pending(app: &App, id: Id<ScheduledJob>, msg: &str) -> crate::Result<bool> {
    tracing::error!(job = %id, "{msg}");
    store::write(app, |db| async move {
        let Some(job) = db.get(id).await? else { return Ok(false) };
        if job.state != JobState::Pending {
            return Ok(false);
        }
        let mut value = job.value;
        let now = app.db().clock().now_ms();
        value.state = JobState::Failed;
        value.completed_at = Some(now);
        value.error = Some(store::truncate(msg.to_string()));
        store::finished(&db, id, &value, WorkResult::Failed { error: msg.to_string() }, now).await?;
        db.replace(id, value).await?;
        Ok(true)
    })
    .await
}

/// Commit `pending` → `inProgress` before an action runs. False if the job is no longer pending.
async fn claim_action(app: &App, id: Id<ScheduledJob>) -> crate::Result<bool> {
    store::write(app, |db| async move {
        let Some(job) = db.get(id).await? else { return Ok(false) };
        if job.state != JobState::Pending {
            return Ok(false);
        }
        let mut value = job.value;
        value.state = JobState::InProgress;
        value.attempts += 1;
        value.started_at = Some(app.db().clock().now_ms());
        db.replace(id, value).await?;
        Ok(true)
    })
    .await
}

/// Run a claimed action and record its outcome (or schedule a retry). A job canceled while it ran keeps its
/// `canceled` state; how the action ended is appended to its `error` (and a value it still returned is kept).
async fn run_action_job(app: &App, job: Doc<ScheduledJob>, registration: &Registration) {
    let signal = &registration.signal;
    if signal.is_canceled() {
        // Canceled between the claim and the start: never started.
        return;
    }
    let run = app.call_action(&job.path, job.args.clone(), &Identity::anonymous(), Origin::Server).await;
    let backoff_base = app.scheduler_state().options.retry_backoff;
    let canceled = signal.is_canceled();
    let recorded = store::write(app, |db| async move {
        let Some(current) = db.get(job.id).await? else { return Ok(()) };
        if current.state == JobState::Canceled && canceled {
            let mut value = current.value;
            let ending = match &run.result {
                Err(e) if e.is::<ActionAborted>() => format!("aborted after the {:?} grace period", signal.grace()),
                Err(e) => format!("it stopped with an error: {e:#}"),
                Ok(result) => {
                    value.result = Some(store::truncate(serde_json::to_string(result)?));
                    "it returned before being aborted".to_string()
                }
            };
            let error = value.error.take().unwrap_or_else(|| "Canceled".to_string());
            value.error = Some(store::truncate(format!("{error}; {ending}")));
            return db.replace(job.id, value).await;
        }
        if current.state != JobState::InProgress {
            return Ok(());
        }
        let now = app.db().clock().now_ms();
        let mut value = current.value;
        match run.result {
            Ok(result) => {
                value.state = JobState::Success;
                value.result = Some(store::truncate(serde_json::to_string(&result)?));
                value.error = None;
                value.completed_at = Some(now);
                store::finished(&db, job.id, &value, WorkResult::Success { return_value: result }, now).await?;
            }
            Err(e) => {
                let error = format!("{e:#}");
                value.error = Some(store::truncate(error.clone()));
                if value.attempts < value.max_attempts {
                    value.state = JobState::Pending;
                    value.run_at = now + retry_delay(&value, backoff_base).as_secs_f64() * 1000.0;
                } else {
                    value.state = JobState::Failed;
                    value.completed_at = Some(now);
                    store::finished(&db, job.id, &value, WorkResult::Failed { error }, now).await?;
                }
            }
        }
        db.replace(job.id, value).await
    })
    .await;
    if let Err(e) = recorded {
        tracing::error!(job = %job.id, "could not record the outcome of a scheduled action: {e:#}");
    }
}

/// How long to wait before the next attempt of an action that just failed attempt number `job.attempts`.
fn retry_delay(job: &ScheduledJob, default_initial: Duration) -> Duration {
    let retry = job.attempts.saturating_sub(1).min(64) as i32;
    let (initial_ms, base) = match job.backoff {
        Some(b) => (b.initial_ms, b.base),
        None => (default_initial.as_secs_f64() * 1000.0, 2.0),
    };
    let ms = initial_ms * base.powi(retry);
    if ms.is_finite() && ms >= 0.0 { Duration::from_secs_f64(ms / 1000.0).min(MAX_BACKOFF) } else { MAX_BACKOFF }
}

/// When the next job or cron is due, or the next cleanup should run. Actions of a full pool don't count: a slot
/// freeing up wakes the loop.
async fn next_wake(app: &App) -> crate::Result<Option<f64>> {
    let st = app.scheduler_state();
    let mut pools = vec![];
    for (pool, slots) in st.all_slots() {
        if slots.semaphore.available_permits() > 0 {
            pools.push(pool);
        }
    }
    let job = store::read(app, |db| async move {
        let mut first = db
            .query::<ScheduledJob>()
            .with_index(ScheduledJob::by_kind_state_run_at, |q| {
                q.eq(ScheduledJob::kind, JobKind::Mutation).eq(ScheduledJob::state, JobState::Pending)
            })
            .first()
            .await?
            .map(|j| j.run_at);
        for pool in pools {
            let action = db
                .query::<ScheduledJob>()
                .with_index(ScheduledJob::by_kind_pool_state_run_at, |q| {
                    q.eq(ScheduledJob::kind, JobKind::Action)
                        .eq(ScheduledJob::pool, pool)
                        .eq(ScheduledJob::state, JobState::Pending)
                })
                .first()
                .await?;
            first = [first, action.map(|j| j.run_at)].into_iter().flatten().reduce(f64::min);
        }
        Ok(first)
    })
    .await?;
    let cron = cron::next_due(app).await?;
    let cleanup = match (st.options.retention, *st.last_cleanup.lock().unwrap_or_else(|e| e.into_inner())) {
        (Some(_), Some(last)) => Some(last + st.options.cleanup_interval.as_secs_f64() * 1000.0),
        _ => None,
    };
    Ok([job, cron, cleanup].into_iter().flatten().reduce(f64::min))
}

/// Did any commit in `(after, up_to]` write `_scheduled_jobs`? (True when the write log can't tell.)
fn jobs_written(app: &App, after: u64, up_to: u64) -> bool {
    let Ok(table) = app.db().catalog().table(ScheduledJob::NAME) else { return true };
    let by_id = table.by_id().id;
    match app.db().write_log().entries_between(after, up_to) {
        Some(entries) => entries.iter().any(|e| e.writes.iter().any(|(index, _)| *index == by_id)),
        None => true,
    }
}

/// The background loop started by `App::start_scheduler`.
async fn run_loop(app: App) {
    let st = app.scheduler_state();
    let mut commits = app.db().write_log().subscribe();
    let mut seen = *commits.borrow_and_update();
    loop {
        // A draining server starts nothing more (`dispatch`): this loop is done, rather than waking for jobs that
        // stay due until the next server takes them.
        if app.is_draining() {
            return;
        }
        let wait = match dispatch(&app).await {
            Ok(_) => match next_wake(&app).await {
                Ok(Some(at)) => {
                    let ms = (at - app.db().clock().now_ms()).max(0.0) + 1.0;
                    Duration::from_secs_f64(ms / 1000.0).min(MAX_SLEEP)
                }
                Ok(None) => MAX_SLEEP,
                Err(e) => {
                    tracing::error!("scheduler: {e:#}");
                    Duration::from_secs(1)
                }
            },
            Err(e) => {
                tracing::error!("scheduler: {e:#}");
                Duration::from_secs(1)
            }
        };
        let sleep = tokio::time::sleep(wait);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                _ = st.notify.notified() => break,
                changed = commits.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let latest = *commits.borrow_and_update();
                    let written = jobs_written(&app, seen, latest);
                    seen = latest;
                    if written {
                        break;
                    }
                }
            }
        }
    }
}
