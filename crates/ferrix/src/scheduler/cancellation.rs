//! Canceling a scheduled action while it runs: cooperative first, then a hard abort.
//!
//! - `ctx.scheduler.cancel(id)` (or the dashboard's `cancel_job`) on an `inProgress` action job commits the job as
//!   `canceled` (with the reason in `error`) and, once that commit is durable, signals the running action.
//! - The action sees it through [`ActionCtx::is_canceled`] and [`ActionCtx::canceled`] (a future to `select!` on) and
//!   can stop cleanly.
//! - If it hasn't returned `SchedulerOptions::cancel_grace` (5s) after the cancel, its future is dropped at its next
//!   `.await` ([`abortable`]): the action, and any nested `ctx.run_action` / `ctx.run_mutation` it is awaiting, stops
//!   there. A nested mutation that is in flight at that moment is rolled back (its transaction never commits); the
//!   ones that already committed **stay committed**: an action is not a transaction. Code that runs without awaiting
//!   (a CPU-bound loop) can't be interrupted; it is dropped at its next `.await`.
//!
//! **At most once.** A canceled job is finished: it is never retried (not even with `with_retries`), a restart
//! doesn't touch it, and the action's late outcome doesn't change its state (it is only noted in `error`, and a
//! value it still returned is kept in `result`).
//!
//! The signal reaches the action through a task-local set by the runner around the job, so it covers the scheduled
//! action and everything it awaits in the same task (nested `ctx.run_action` calls included). Actions called by
//! clients or by other code paths are never canceled: `is_canceled()` is always false there.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tokio::time::Instant;

use super::tables::ScheduledJob;
use crate::id::Id;
use crate::runtime::ActionCtx;

tokio::task_local! {
    /// The cancel signal of the scheduled action job running in this task.
    static CURRENT: Arc<CancelSignal>;
}

/// Running action jobs by id. Process-wide (job ids are random, so two `App`s never share one); entries live exactly
/// as long as the job's action runs ([`Registration`]).
static RUNNING: LazyLock<Mutex<HashMap<Id<ScheduledJob>, Arc<CancelSignal>>>> = LazyLock::new(Default::default);

/// The cancel state of one running action job.
pub(crate) struct CancelSignal {
    /// `Some((reason, when))` once canceled.
    state: watch::Sender<Option<(String, Instant)>>,
    grace: Duration,
}

impl CancelSignal {
    fn new(grace: Duration) -> CancelSignal {
        CancelSignal { state: watch::Sender::new(None), grace }
    }

    /// Request cancellation (the first reason wins).
    pub(crate) fn cancel(&self, reason: &str) {
        self.state.send_if_modified(|state| match state {
            Some(_) => false,
            None => {
                *state = Some((reason.to_string(), Instant::now()));
                true
            }
        });
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.state.borrow().is_some()
    }

    /// Resolves once canceled.
    async fn canceled(&self) {
        let mut rx = self.state.subscribe();
        let _ = rx.wait_for(Option::is_some).await;
    }

    /// Resolves `grace` after the cancel.
    async fn abort_deadline(&self) {
        let mut rx = self.state.subscribe();
        let at = match rx.wait_for(Option::is_some).await {
            Ok(state) => state.as_ref().map(|(_, at)| *at),
            Err(_) => None,
        };
        match at {
            Some(at) => tokio::time::sleep_until(at + self.grace).await,
            None => std::future::pending().await,
        }
    }

    pub(crate) fn grace(&self) -> Duration {
        self.grace
    }
}

/// Keeps a running job in [`RUNNING`]; removes it when dropped.
pub(crate) struct Registration {
    id: Id<ScheduledJob>,
    pub(crate) signal: Arc<CancelSignal>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if running.get(&self.id).is_some_and(|s| Arc::ptr_eq(s, &self.signal)) {
            running.remove(&self.id);
        }
    }
}

/// Register an action job that is about to run (before it is claimed, so a cancel committed right after the claim
/// finds it).
pub(crate) fn register(id: Id<ScheduledJob>, grace: Duration) -> Registration {
    let signal = Arc::new(CancelSignal::new(grace));
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).insert(id, signal.clone());
    Registration { id, signal }
}

/// Signal the running action of job `id`, if it runs in this process. Called after the cancel committed.
pub(crate) fn signal(id: Id<ScheduledJob>, reason: &str) {
    let signal = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    if let Some(signal) = signal {
        signal.cancel(reason);
    }
}

/// Run `fut` (a scheduled action job) with `signal` as the task's cancel signal.
pub(crate) async fn scope<F: Future>(signal: Arc<CancelSignal>, fut: F) -> F::Output {
    CURRENT.scope(signal, fut).await
}

/// The error of an action whose future was dropped because it was still running `grace` after its job was canceled.
#[derive(Debug)]
pub struct ActionAborted {
    pub grace: Duration,
}

impl std::fmt::Display for ActionAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "canceled: the action was aborted {:?} after its job was canceled", self.grace)
    }
}

impl std::error::Error for ActionAborted {}

/// An action's handler future, dropped (at its next `.await`) `grace` after its job is canceled. Outside a scheduled
/// action job, just `fut`. Used by the runtime around every action handler.
pub(crate) async fn abortable(fut: impl Future<Output = crate::Result<Value>>) -> crate::Result<Value> {
    let Ok(signal) = CURRENT.try_with(Arc::clone) else { return fut.await };
    tokio::select! {
        biased;
        result = fut => result,
        () = signal.abort_deadline() => Err(ActionAborted { grace: signal.grace }.into()),
    }
}

impl ActionCtx {
    /// Whether this action runs as a scheduled job that has been canceled (`ctx.scheduler.cancel(id)`, the
    /// dashboard). Always false for actions called any other way. A canceled action should stop soon: it is aborted
    /// at its next `.await` once `SchedulerOptions::cancel_grace` (5s) has passed.
    pub fn is_canceled(&self) -> bool {
        CURRENT.try_with(|s| s.is_canceled()).unwrap_or(false)
    }

    /// Resolves when this action's job is canceled (never, for actions not run by the scheduler). For `select!`:
    ///
    /// ```ignore
    /// tokio::select! {
    ///     _ = ctx.canceled() => return Ok(Progress::Stopped),
    ///     page = fetch_next_page(&client) => { /* ... */ }
    /// }
    /// ```
    pub async fn canceled(&self) {
        match CURRENT.try_with(Arc::clone) {
            Ok(signal) => signal.canceled().await,
            Err(_) => std::future::pending().await,
        }
    }
}
