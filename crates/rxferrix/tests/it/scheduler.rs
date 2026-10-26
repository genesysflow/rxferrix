//! The scheduler and crons: transactional scheduling, exactly-once mutations (also across a simulated crash),
//! at-most-once actions with restart recovery and opt-in retries, cancel, same-time ordering, the concurrency
//! limit, crons (next run, missed-run catch-up, no pile-up, reconcile), retention, the admin-only dashboard
//! functions, the execution log's `scheduler` caller, and reactivity over a real socket.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rxferrix::auth::AuthConfig;
use rxferrix::scheduler::{CancelOutcome, JobState, ScheduledJob, SchedulerOptions};
use rxferrix::storage::StorageConfig;
use rxferrix::testing::TestApp;
use rxferrix::{AppOptions, Doc, FunctionRef, Id, LogFilter};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

mod app {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use rxferrix::prelude::*;
    use rxferrix::scheduler::ScheduledJob;
    use tokio::sync::Notify;

    #[table(name = "sched_notes")]
    pub struct Note {
        pub text: String,
    }

    #[query]
    pub async fn notes(ctx: &QueryCtx) -> Result<Vec<Doc<Note>>> {
        ctx.db.query::<Note>().collect().await
    }

    #[query]
    pub async fn get_job(ctx: &QueryCtx, id: Id<ScheduledJob>) -> Result<Option<Doc<ScheduledJob>>> {
        ctx.db.get(id).await
    }

    #[query]
    pub async fn all_jobs(ctx: &QueryCtx) -> Result<Vec<Doc<ScheduledJob>>> {
        ctx.db.query::<ScheduledJob>().collect().await
    }

    /// Internal: codegen would otherwise emit `Doc<"_crons">`, a table apps can't reference.
    #[internal_query]
    pub async fn all_crons(ctx: &QueryCtx) -> Result<Vec<Doc<rxferrix::scheduler::Cron>>> {
        ctx.db.query::<rxferrix::scheduler::Cron>().collect().await
    }

    #[internal_mutation]
    pub async fn add_note(ctx: &MutationCtx, text: String) -> Result<Id<Note>> {
        ctx.db.insert(Note { text }).await
    }

    /// Adds a note now and schedules another one after `delay_ms`; with `fail`, then fails (rolling both back).
    #[mutation]
    pub async fn add_and_schedule(
        ctx: &MutationCtx,
        text: String,
        delay_ms: u64,
        fail: bool,
    ) -> Result<Id<ScheduledJob>> {
        ctx.db.insert(Note { text: text.clone() }).await?;
        let job =
            ctx.scheduler.run_after::<add_note>(Duration::from_millis(delay_ms), (format!("{text} (later)"),)).await?;
        if fail {
            return Err(UserError::new("nope", "failing on purpose").into());
        }
        Ok(job)
    }

    #[mutation]
    pub async fn schedule_at(ctx: &MutationCtx, text: String, at: f64) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.run_at::<add_note>(at, (text,)).await
    }

    /// Schedules a note at `at` for each label, in order, in one transaction.
    #[mutation]
    pub async fn schedule_many(ctx: &MutationCtx, labels: Vec<String>, at: f64) -> Result<()> {
        for label in labels {
            ctx.scheduler.run_at::<add_note>(at, (label,)).await?;
        }
        Ok(())
    }

    #[mutation]
    pub async fn schedule_and_cancel(ctx: &MutationCtx, text: String) -> Result<Id<ScheduledJob>> {
        let id = ctx.scheduler.run_after::<add_note>(Duration::ZERO, (text,)).await?;
        ctx.scheduler.cancel(id).await?;
        Ok(id)
    }

    #[mutation]
    pub async fn cancel(ctx: &MutationCtx, id: Id<ScheduledJob>) -> Result<()> {
        ctx.scheduler.cancel(id).await
    }

    #[mutation]
    pub async fn schedule_failing(ctx: &MutationCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.run_after::<failing_add>(Duration::ZERO, ("doomed".to_string(),)).await
    }

    #[internal_mutation]
    pub async fn failing_add(ctx: &MutationCtx, text: String) -> Result<()> {
        ctx.db.insert(Note { text }).await?;
        Err(anyhow::anyhow!("the scheduled mutation broke"))
    }

    /// Records whether it ran with an identity.
    #[internal_mutation]
    pub async fn whoami(ctx: &MutationCtx) -> Result<()> {
        let who = if ctx.auth.identity().is_anonymous() { "anonymous" } else { "someone" };
        ctx.db.insert(Note { text: who.to_string() }).await?;
        Ok(())
    }

    #[mutation]
    pub async fn schedule_whoami(ctx: &MutationCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.run_after::<whoami>(Duration::ZERO, ()).await
    }

    // ---- crash simulation: a mutation that hangs (after writing) while CRASH is set

    pub static CRASH: AtomicBool = AtomicBool::new(false);
    pub static CRASH_STARTED: Notify = Notify::const_new();

    #[internal_mutation]
    pub async fn crashy_add(ctx: &MutationCtx, text: String) -> Result<()> {
        ctx.db.insert(Note { text }).await?;
        if CRASH.load(Ordering::SeqCst) {
            CRASH_STARTED.notify_one();
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
        Ok(())
    }

    #[mutation]
    pub async fn schedule_crashy(ctx: &MutationCtx, text: String) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.run_after::<crashy_add>(Duration::ZERO, (text,)).await
    }

    // ---- actions

    #[action]
    pub async fn act_add(ctx: &ActionCtx, text: String) -> Result<String> {
        ctx.run_mutation::<add_note>((text.clone(),)).await?;
        Ok(format!("added {text}"))
    }

    #[internal_action]
    pub async fn act_fail(_ctx: &ActionCtx) -> Result<()> {
        Err(anyhow::anyhow!("the action broke"))
    }

    #[mutation]
    pub async fn schedule_action(ctx: &MutationCtx, text: String, fail: bool) -> Result<Id<ScheduledJob>> {
        if fail {
            ctx.scheduler.run_after::<act_fail>(Duration::ZERO, ()).await
        } else {
            ctx.scheduler.run_after::<act_add>(Duration::ZERO, (text,)).await
        }
    }

    /// Schedules from an action (immediately, through `_system/scheduler:schedule`), and cancels the second job.
    #[action]
    pub async fn act_schedule(ctx: &ActionCtx, text: String) -> Result<Vec<Id<ScheduledJob>>> {
        let a = ctx.scheduler.run_after::<add_note>(Duration::from_secs(10), (text.clone(),)).await?;
        let b = ctx.scheduler.run_after::<add_note>(Duration::from_secs(10), (format!("{text} (canceled)"),)).await?;
        ctx.scheduler.cancel(b).await?;
        Ok(vec![a, b])
    }

    pub static FLAKY_FAILURES: AtomicUsize = AtomicUsize::new(0);

    /// Fails its first two runs.
    #[internal_action]
    pub async fn flaky(_ctx: &ActionCtx) -> Result<u32> {
        let n = FLAKY_FAILURES.fetch_add(1, Ordering::SeqCst);
        if n < 2 {
            return Err(anyhow::anyhow!("flaky failure #{}", n + 1));
        }
        Ok(n as u32 + 1)
    }

    #[mutation]
    pub async fn schedule_flaky(ctx: &MutationCtx, retries: u32) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.with_retries(retries).run_after::<flaky>(Duration::ZERO, ()).await
    }

    pub static RUNNING: AtomicUsize = AtomicUsize::new(0);
    pub static MAX_RUNNING: AtomicUsize = AtomicUsize::new(0);

    #[internal_action]
    pub async fn slow(_ctx: &ActionCtx) -> Result<()> {
        let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_RUNNING.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }

    #[mutation]
    pub async fn schedule_slow(ctx: &MutationCtx, n: u32) -> Result<()> {
        for _ in 0..n {
            ctx.scheduler.run_after::<slow>(Duration::ZERO, ()).await?;
        }
        Ok(())
    }

    /// An action that waits until released (`HANG_STARTED` is notified when it starts).
    pub static HANG_STARTED: Notify = Notify::const_new();
    pub static HANG_RELEASE: Notify = Notify::const_new();

    #[internal_action]
    pub async fn hang(_ctx: &ActionCtx) -> Result<()> {
        HANG_STARTED.notify_one();
        HANG_RELEASE.notified().await;
        Ok(())
    }

    #[mutation]
    pub async fn schedule_hang(ctx: &MutationCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.run_after::<hang>(Duration::ZERO, ()).await
    }

    /// The same for the cron pile-up test.
    pub static CRON_HANG_STARTED: Notify = Notify::const_new();
    pub static CRON_HANG_RELEASE: Notify = Notify::const_new();

    /// Only the first run blocks: the cron (every second of *real* time, as the clock follows it) may fire again after
    /// the test releases it, and a second blocking run would hang the test's final `run_due_jobs` forever.
    pub static CRON_HANG_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    #[internal_action]
    pub async fn cron_hang(_ctx: &ActionCtx) -> Result<()> {
        if CRON_HANG_ARMED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            CRON_HANG_STARTED.notify_one();
            CRON_HANG_RELEASE.notified().await;
        }
        Ok(())
    }

    // ---- canceling running actions

    pub static COOP_STARTED: Notify = Notify::const_new();

    /// Writes a note, then waits; once canceled, writes another and returns.
    #[internal_action]
    pub async fn cooperative(ctx: &ActionCtx) -> Result<String> {
        ctx.run_mutation::<add_note>(("coop: before".to_string(),)).await?;
        assert!(!ctx.is_canceled());
        COOP_STARTED.notify_one();
        tokio::select! {
            _ = ctx.canceled() => {
                assert!(ctx.is_canceled());
                ctx.run_mutation::<add_note>(("coop: cleaned up".to_string(),)).await?;
                Ok("stopped".to_string())
            }
            _ = tokio::time::sleep(Duration::from_secs(3600)) => Ok("finished".to_string()),
        }
    }

    #[mutation]
    pub async fn schedule_cooperative(ctx: &MutationCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.run_after::<cooperative>(Duration::ZERO, ()).await
    }

    pub static STUBBORN_STARTED: Notify = Notify::const_new();

    /// Ignores cancellation: once canceled it even starts a mutation that hangs (holding the writer) until the
    /// action is aborted, which rolls it back.
    #[internal_action]
    pub async fn stubborn(ctx: &ActionCtx) -> Result<()> {
        ctx.run_mutation::<add_note>(("stubborn: before".to_string(),)).await?;
        STUBBORN_STARTED.notify_one();
        ctx.canceled().await;
        ctx.run_mutation::<hanging_add>(("stubborn: in flight".to_string(),)).await?;
        Ok(())
    }

    #[internal_mutation]
    pub async fn hanging_add(ctx: &MutationCtx, text: String) -> Result<()> {
        ctx.db.insert(Note { text }).await?;
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(())
    }

    #[mutation]
    pub async fn schedule_stubborn(ctx: &MutationCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.with_retries(3).run_after::<stubborn>(Duration::ZERO, ()).await
    }

    /// Waits for a cancellation (which never comes outside a scheduled job), reporting `is_canceled()` first.
    #[action]
    pub async fn wait_for_cancel(ctx: &ActionCtx) -> Result<bool> {
        assert!(!ctx.is_canceled());
        ctx.canceled().await;
        Ok(ctx.is_canceled())
    }

    /// Cancels a job from an action, with a reason.
    #[action]
    pub async fn act_cancel(ctx: &ActionCtx, id: Id<ScheduledJob>, reason: String) -> Result<()> {
        ctx.scheduler.cancel_with_reason(id, &reason).await
    }

    pub static DASH_STARTED: Notify = Notify::const_new();

    #[internal_action]
    pub async fn dash_hang(ctx: &ActionCtx) -> Result<()> {
        DASH_STARTED.notify_one();
        ctx.canceled().await;
        Err(anyhow::anyhow!("gave up"))
    }

    #[internal_mutation]
    pub async fn cron_tick(ctx: &MutationCtx, label: String) -> Result<()> {
        ctx.db.insert(Note { text: format!("cron:{label}") }).await?;
        Ok(())
    }
}

use app::*;

fn texts(notes: &[Doc<Note>]) -> Vec<String> {
    notes.iter().map(|n| n.text.clone()).collect()
}

async fn job(t: &TestApp, id: Id<ScheduledJob>) -> Doc<ScheduledJob> {
    t.query::<get_job>((id,)).await.unwrap().expect("job exists")
}

#[tokio::test]
async fn transactional_scheduling_runs_after_the_delay_exactly_once() {
    let t = TestApp::new().await.unwrap();

    // A rolled-back mutation schedules nothing.
    let err = t.mutation::<add_and_schedule>(("x".to_string(), 0, true)).await.unwrap_err();
    assert!(err.to_string().contains("failing on purpose"));
    assert!(t.query::<all_jobs>(()).await.unwrap().is_empty());
    assert!(t.query::<notes>(()).await.unwrap().is_empty());

    // A committed one schedules its job, which runs once it's due.
    let id = t.mutation::<add_and_schedule>(("a".to_string(), 60_000, false)).await.unwrap();
    let pending = job(&t, id).await;
    assert_eq!(
        (pending.state, pending.attempts, pending.origin.as_deref()),
        (JobState::Pending, 0, Some(add_and_schedule::path().as_str()))
    );
    assert_eq!(pending.path, add_note::path());
    assert_eq!(pending.args, json!({ "text": "a (later)" }));
    assert!((pending.run_at - pending.scheduled_at - 60_000.0).abs() < 1.0);

    assert_eq!(t.run_due_jobs().await.unwrap(), 0, "not due yet");
    assert_eq!(t.advance_time(Duration::from_secs(30)).await.unwrap(), 0);
    assert_eq!(t.advance_time(Duration::from_secs(31)).await.unwrap(), 1);
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["a", "a (later)"]);
    let done = job(&t, id).await;
    assert_eq!((done.state, done.attempts), (JobState::Success, 1));
    assert!(
        done.result.as_deref().unwrap().starts_with('"'),
        "the result is the new note's id as JSON: {:?}",
        done.result
    );
    assert!(done.started_at.is_some() && done.completed_at.is_some() && done.error.is_none());

    // Never again.
    assert_eq!(t.advance_time(Duration::from_secs(3600)).await.unwrap(), 0);
    assert_eq!(t.query::<notes>(()).await.unwrap().len(), 2);
    let t = t.restart().await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 0);
    assert_eq!(t.query::<notes>(()).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_failing_scheduled_mutation_rolls_back_and_is_marked_failed() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<schedule_failing>(()).await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 1);
    let failed = job(&t, id).await;
    assert_eq!((failed.state, failed.attempts), (JobState::Failed, 1));
    assert!(failed.error.as_deref().unwrap().contains("the scheduled mutation broke"), "{:?}", failed.error);
    assert!(failed.completed_at.is_some());
    assert!(t.query::<notes>(()).await.unwrap().is_empty(), "its write was rolled back");
    assert_eq!(t.run_due_jobs().await.unwrap(), 0, "not retried");
}

#[tokio::test]
async fn a_crash_mid_mutation_leaves_the_job_pending_and_it_then_runs_once() {
    let t = TestApp::with_options(AppOptions {
        dev: true,
        mutation_timeout: Duration::from_secs(3600),
        ..Default::default()
    })
    .await
    .unwrap();
    let id = t.mutation::<schedule_crashy>(("once".to_string(),)).await.unwrap();

    // The job's mutation has written its note and hangs; the "process" dies (the runner's future is dropped).
    CRASH.store(true, Ordering::SeqCst);
    let app = t.app().clone();
    let runner = tokio::spawn(async move { app.run_due_jobs().await });
    tokio::time::timeout(Duration::from_secs(5), CRASH_STARTED.notified()).await.expect("job started");
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    CRASH.store(false, Ordering::SeqCst);

    let after_crash = job(&t, id).await;
    assert_eq!(
        (after_crash.state, after_crash.attempts),
        (JobState::Pending, 0),
        "nothing of the attempt was committed"
    );
    assert!(t.query::<notes>(()).await.unwrap().is_empty());

    // After the "restart" it runs, exactly once.
    let t = t.restart().await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 1);
    assert_eq!(t.run_due_jobs().await.unwrap(), 0);
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["once"]);
    let done = job(&t, id).await;
    assert_eq!((done.state, done.attempts), (JobState::Success, 1));
    assert_eq!(t.app().db().verify_indexes().await.unwrap(), Vec::<String>::new());
}

#[tokio::test]
async fn cancel_in_mutations_and_actions() {
    let t = TestApp::new().await.unwrap();
    // Cancel later.
    let id = t.mutation::<add_and_schedule>(("c".to_string(), 60_000, false)).await.unwrap();
    t.mutation::<cancel>((id,)).await.unwrap();
    let canceled = job(&t, id).await;
    assert_eq!(canceled.state, JobState::Canceled);
    assert!(canceled.completed_at.is_some());
    assert_eq!(t.advance_time(Duration::from_secs(120)).await.unwrap(), 0);
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["c"], "the canceled job never ran");
    // Canceling again (or a finished job) does nothing.
    t.mutation::<cancel>((id,)).await.unwrap();

    // Schedule and cancel in one transaction.
    let id = t.mutation::<schedule_and_cancel>(("never".to_string(),)).await.unwrap();
    assert_eq!(job(&t, id).await.state, JobState::Canceled);
    assert_eq!(t.run_due_jobs().await.unwrap(), 0);

    // From an action: takes effect immediately.
    let [a, b] = t.action::<act_schedule>(("from action".to_string(),)).await.unwrap()[..] else { panic!("two ids") };
    let (ja, jb) = (job(&t, a).await, job(&t, b).await);
    assert_eq!((ja.state, jb.state), (JobState::Pending, JobState::Canceled));
    assert_eq!(ja.origin.as_deref(), Some(act_schedule::path().as_str()));
    assert_eq!(t.advance_time(Duration::from_secs(11)).await.unwrap(), 1);
    assert!(texts(&t.query::<notes>(()).await.unwrap()).contains(&"from action".to_string()));

    // Once deleted (after the retention period), canceling is a `not_found` user error.
    t.advance_time(Duration::from_secs(8 * 24 * 3600)).await.unwrap();
    let err = t.mutation::<cancel>((id,)).await.unwrap_err();
    let user_error = err.downcast_ref::<rxferrix::UserError>().expect("a user error");
    assert_eq!(user_error.code, "not_found");
}

#[tokio::test]
async fn jobs_due_at_the_same_time_run_in_scheduling_order() {
    let t = TestApp::new().await.unwrap();
    let at = t.app().db().clock().now_ms() + 1000.0;
    let labels = |r: std::ops::Range<u32>| r.map(|i| format!("{i:02}")).collect::<Vec<_>>();
    t.mutation::<schedule_many>((labels(0..5), at)).await.unwrap();
    t.mutation::<schedule_at>(("early".to_string(), at - 500.0)).await.unwrap();
    t.mutation::<schedule_many>((labels(5..10), at)).await.unwrap();
    t.mutation::<schedule_at>(("late".to_string(), at + 500.0)).await.unwrap();
    assert_eq!(t.advance_time(Duration::from_secs(2)).await.unwrap(), 12);
    let mut expected = vec!["early".to_string()];
    expected.extend(labels(0..10));
    expected.push("late".into());
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), expected);
}

#[tokio::test]
async fn actions_run_at_most_once_and_are_failed_by_a_restart() {
    let t = TestApp::new().await.unwrap();
    let ok = t.mutation::<schedule_action>(("act".to_string(), false)).await.unwrap();
    let bad = t.mutation::<schedule_action>((String::new(), true)).await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 2);
    let ok = job(&t, ok).await;
    assert_eq!((ok.state, ok.attempts, ok.result.as_deref()), (JobState::Success, 1, Some("\"added act\"")));
    let bad = job(&t, bad).await;
    assert_eq!((bad.state, bad.attempts), (JobState::Failed, 1));
    assert!(bad.error.as_deref().unwrap().contains("the action broke"));
    assert_eq!(t.run_due_jobs().await.unwrap(), 0, "failed actions are not retried by default");
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["act"]);

    // An action is in progress when the process stops: after the restart it is failed, never run again.
    let id = t.mutation::<schedule_hang>(()).await.unwrap();
    let app = t.app().clone();
    let runner = tokio::spawn(async move { app.run_due_jobs().await });
    tokio::time::timeout(Duration::from_secs(5), HANG_STARTED.notified()).await.expect("action started");
    let running = job(&t, id).await;
    assert_eq!((running.state, running.attempts), (JobState::InProgress, 1));
    runner.abort();
    let _ = runner.await;
    let t = t.restart().await.unwrap();
    let interrupted = job(&t, id).await;
    assert_eq!(interrupted.state, JobState::Failed);
    assert_eq!(interrupted.error.as_deref(), Some(rxferrix::scheduler::runner::INTERRUPTED));
    assert_eq!(t.run_due_jobs().await.unwrap(), 0);
    // The old process's action finishing late doesn't overwrite that.
    HANG_RELEASE.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(job(&t, id).await.state, JobState::Failed);
}

#[tokio::test]
async fn actions_can_opt_into_retries_with_backoff() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<schedule_flaky>((2,)).await.unwrap();
    assert_eq!(job(&t, id).await.max_attempts, 3);
    assert_eq!(t.run_due_jobs().await.unwrap(), 1);
    let first = job(&t, id).await;
    assert_eq!((first.state, first.attempts), (JobState::Pending, 1), "back to pending after the first failure");
    assert!(first.error.as_deref().unwrap().contains("flaky failure #1"));
    assert!(first.run_at > t.app().db().clock().now_ms() + 500.0, "1s backoff");
    assert_eq!(t.advance_time(Duration::from_millis(1100)).await.unwrap(), 1);
    let second = job(&t, id).await;
    assert_eq!((second.state, second.attempts), (JobState::Pending, 2));
    assert_eq!(t.advance_time(Duration::from_millis(1100)).await.unwrap(), 0, "the backoff doubled to 2s");
    assert_eq!(t.advance_time(Duration::from_millis(1000)).await.unwrap(), 1);
    let third = job(&t, id).await;
    assert_eq!((third.state, third.attempts, third.result.as_deref()), (JobState::Success, 3, Some("3")));
    assert!(third.error.is_none());
}

#[tokio::test]
async fn the_concurrency_limit_bounds_running_actions() {
    let options = AppOptions {
        dev: true,
        scheduler: SchedulerOptions { concurrency: 2, ..Default::default() },
        ..Default::default()
    };
    let t = TestApp::with_options(options).await.unwrap();
    t.mutation::<schedule_slow>((6,)).await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 6);
    assert_eq!(MAX_RUNNING.load(Ordering::SeqCst), 2);
    assert!(t.query::<all_jobs>(()).await.unwrap().iter().all(|j| j.state == JobState::Success));
}

#[tokio::test]
async fn scheduled_functions_run_without_identity_and_log_as_scheduler() {
    let t = TestApp::new().await.unwrap();
    let someone = rxferrix::Identity { user_id: Some("someone".into()), ..Default::default() };
    t.mutation_as::<schedule_whoami>(&someone, ()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["anonymous"]);
    t.app().logs().flush().await.unwrap();
    let logs =
        t.app().logs().query(LogFilter { identity: Some("scheduler".into()), ..Default::default() }).await.unwrap();
    assert!(logs.iter().any(|e| e.path == whoami::path()), "{logs:?}");
    // Hiding background work leaves out the job, but not the user's call that scheduled it.
    let foreground = t.app().logs().query(LogFilter { exclude_background: true, ..Default::default() }).await.unwrap();
    assert!(foreground.iter().all(|e| e.path != whoami::path()), "{foreground:?}");
    assert!(foreground.iter().any(|e| e.path == schedule_whoami::path()), "{foreground:?}");
}

fn crons_options(crons: SchedulerOptions) -> AppOptions {
    // Without the storage sweep's system cron, so the counts of jobs that ran are the app's crons only.
    AppOptions {
        dev: true,
        scheduler: crons,
        storage: StorageConfig::default().sweep_interval(None),
        ..Default::default()
    }
}

/// The app's `_crons` rows (without rxferrix's system crons, e.g. `_storage_sweep`).
async fn cron_rows(t: &TestApp) -> Vec<Value> {
    let rows = t.query::<all_crons>(()).await.unwrap();
    rows.into_iter().filter(|c| !c.name.starts_with('_')).map(|c| serde_json::to_value(c).unwrap()).collect()
}

#[tokio::test]
async fn crons_fire_on_schedule_catch_up_once_and_reconcile() {
    let every_minute = SchedulerOptions::default()
        .cron::<cron_tick>("tick", "0 * * * * *", ("a".to_string(),))
        .interval::<cron_tick>("every-hour", Duration::from_secs(3600), ("h".to_string(),));
    let t = TestApp::with_options(crons_options(every_minute.clone())).await.unwrap();
    let rows = cron_rows(&t).await;
    assert_eq!(rows.len(), 2);
    let tick = rows.iter().find(|r| r["name"] == "tick").unwrap().clone();
    let now = t.app().db().clock().now_ms();
    let next = tick["nextRun"].as_f64().unwrap();
    assert!(next > now && next <= now + 60_000.0 && next % 1000.0 == 0.0, "next minute: {tick}");
    assert_eq!(tick["schedule"], "0 * * * * *");
    let hourly = rows.iter().find(|r| r["name"] == "every-hour").unwrap();
    assert_eq!(hourly["schedule"], "every 1h");

    assert_eq!(t.run_due_jobs().await.unwrap(), 0);
    assert_eq!(t.advance_time(Duration::from_secs(60)).await.unwrap(), 1);
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["cron:a"]);
    let jobs = t.query::<all_jobs>(()).await.unwrap();
    assert_eq!(jobs[0].origin.as_deref(), Some("cron:tick"));
    let tick = cron_rows(&t).await.into_iter().find(|r| r["name"] == "tick").unwrap();
    assert_eq!(tick["lastJob"], json!(jobs[0].id));
    assert!(tick["lastRun"].as_f64().is_some());

    // Ten minutes of downtime: the missed runs run once at startup, not ten times.
    t.app().db().clock().advance(Duration::from_secs(600));
    let t = t.restart().await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 1);
    let ticks = texts(&t.query::<notes>(()).await.unwrap()).iter().filter(|n| *n == "cron:a").count();
    assert_eq!(ticks, 2);
    let next = cron_rows(&t).await.into_iter().find(|r| r["name"] == "tick").unwrap()["nextRun"].as_f64().unwrap();
    assert!(next > t.app().db().clock().now_ms(), "the next run is computed from now");
    // A 50-minute jump is like downtime too: the minutely cron runs once, and the hourly interval (due an hour
    // after the first start) once.
    assert_eq!(t.advance_time(Duration::from_secs(50 * 60)).await.unwrap(), 2);
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()).iter().filter(|n| *n == "cron:h").count(), 1);

    // Redeploy: "every-hour" is gone, "tick" changed its schedule.
    let changed = SchedulerOptions::default().cron::<cron_tick>("tick", "0 0 * * * *", ("a".to_string(),));
    let t = t.restart_with(crons_options(changed)).await.unwrap();
    let rows = cron_rows(&t).await;
    assert_eq!(rows.len(), 1, "removed crons are deleted: {rows:?}");
    assert_eq!(rows[0]["schedule"], "0 0 * * * *");
    let next = rows[0]["nextRun"].as_f64().unwrap();
    assert_eq!(next % 3_600_000.0, 0.0, "recomputed for the new schedule");
}

#[tokio::test]
async fn cron_runs_are_skipped_while_the_previous_one_is_still_running() {
    let options = crons_options(SchedulerOptions::default().cron::<cron_hang>("hang", "* * * * * *", ()));
    let t = TestApp::with_options(options).await.unwrap();
    let app = t.app().clone();
    t.app().db().clock().advance(Duration::from_secs(1));
    let first = tokio::spawn(async move { app.run_due_jobs().await });
    tokio::time::timeout(Duration::from_secs(5), CRON_HANG_STARTED.notified()).await.expect("cron job started");

    // Three more runs come due while the first is still in progress: all skipped. Each skip moves the cron's next run;
    // wait for that change itself, not for a fixed delay or a comparison with "now" (the clock follows real time, so
    // on a loaded machine "now" can pass a one-second cron's next run at any point).
    let mut last_due = 0.0;
    for _ in 0..3 {
        let before = cron_rows(&t).await[0]["nextRun"].as_f64().unwrap();
        t.app().db().clock().advance(Duration::from_secs(1));
        last_due = t.app().db().clock().now_ms();
        let app = t.app().clone();
        let again = tokio::spawn(async move { app.run_due_jobs().await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while cron_rows(&t).await[0]["nextRun"].as_f64().unwrap() == before {
            assert!(tokio::time::Instant::now() < deadline, "the due cron run was never skipped");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!again.is_finished(), "waits for the running action");
        again.abort();
    }
    let jobs = t.query::<all_jobs>(()).await.unwrap();
    assert_eq!(jobs.len(), 1, "no pile-up: {jobs:?}");
    assert_eq!(jobs[0].state, JobState::InProgress);
    let row = cron_rows(&t).await.remove(0);
    // Compared with when the last run came due, not with "now": the clock follows real time, and this cron fires every
    // second, so on a loaded machine "now" can pass the next run before this line.
    assert!(row["nextRun"].as_f64().unwrap() > last_due, "skipped runs still advance nextRun");

    CRON_HANG_RELEASE.notify_one();
    // At least the held run; the real-time cron may have come due again meanwhile (those runs don't block).
    let ran =
        tokio::time::timeout(Duration::from_secs(30), first).await.expect("run_due_jobs finished").unwrap().unwrap();
    assert!(ran >= 1, "{ran}");
    let jobs = t.query::<all_jobs>(()).await.unwrap();
    assert!(jobs.iter().all(|j| j.state == JobState::Success), "{jobs:?}");
}

#[tokio::test]
async fn invalid_crons_fail_startup() {
    let bad =
        |s: SchedulerOptions| async move { TestApp::with_options(crons_options(s)).await.err().unwrap().to_string() };
    let msg = bad(SchedulerOptions::default().cron::<cron_tick>("x", "*/5 * * * *", ("a".to_string(),))).await;
    assert!(msg.contains("has 5 fields"), "{msg}");
    let msg = bad(SchedulerOptions::default().cron::<cron_tick>("x", "0 61 * * * *", ("a".to_string(),))).await;
    assert!(msg.contains("invalid expression"), "{msg}");
    let msg = bad(SchedulerOptions::default()
        .interval::<cron_tick>("x", Duration::from_secs(1), ("a".to_string(),))
        .interval::<cron_tick>("x", Duration::from_secs(2), ("b".to_string(),)))
    .await;
    assert!(msg.contains("two crons are named `x`"), "{msg}");
    let msg = bad(SchedulerOptions::default().interval::<cron_tick>("x", Duration::ZERO, ("a".to_string(),))).await;
    assert!(msg.contains("longer than zero"), "{msg}");
}

#[tokio::test]
async fn finished_jobs_are_deleted_after_the_retention_period() {
    // Without the storage sweep's system cron, whose jobs would run (and be kept) over these days too.
    let storage = rxferrix::storage::StorageConfig::default().sweep_interval(None);
    let t = TestApp::with_options(AppOptions { dev: true, storage, ..Default::default() }).await.unwrap();
    let done = t.mutation::<add_and_schedule>(("r".to_string(), 0, false)).await.unwrap();
    let canceled = t.mutation::<schedule_and_cancel>(("never".to_string(),)).await.unwrap();
    let failed = t.mutation::<schedule_failing>(()).await.unwrap();
    let far = t.mutation::<add_and_schedule>(("far".to_string(), 30 * 24 * 3600 * 1000, false)).await.unwrap();
    t.run_due_jobs().await.unwrap();
    assert_eq!(t.advance_time(Duration::from_secs(6 * 24 * 3600)).await.unwrap(), 0);
    assert_eq!(t.query::<all_jobs>(()).await.unwrap().len(), 4, "kept for 7 days");
    t.advance_time(Duration::from_secs(2 * 24 * 3600)).await.unwrap();
    let left: Vec<_> = t.query::<all_jobs>(()).await.unwrap().into_iter().map(|j| j.id).collect();
    assert_eq!(left, vec![far], "only the pending job is left");
    for id in [done, canceled, failed] {
        assert!(t.query::<get_job>((id,)).await.unwrap().is_none());
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Dashboard functions and reactivity, over real HTTP / WebSockets
// ---------------------------------------------------------------------------------------------------------------------

struct Http {
    client: reqwest::Client,
    base: String,
}

impl Http {
    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.client.post(format!("{}{path}", self.base)).json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.unwrap();
        (res.status().as_u16(), res.json().await.unwrap_or(Value::Null))
    }

    async fn call(&self, kind: &str, name: &str, args: Value, token: Option<&str>) -> (u16, Value) {
        self.post(&format!("/api/{kind}"), token, json!({ "path": format!("_system/scheduler:{name}"), "args": args }))
            .await
    }

    async fn ok(&self, kind: &str, name: &str, args: Value, token: &str) -> Value {
        let (s, b) = self.call(kind, name, args, Some(token)).await;
        assert_eq!(s, 200, "{name}: {b}");
        b["value"].clone()
    }
}

fn code_of(v: &Value) -> &str {
    v["errorData"]["code"].as_str().unwrap_or("")
}

fn page(state: Option<&str>) -> Value {
    json!({ "state": state, "paginationOpts": { "numItems": 50, "cursor": null } })
}

#[tokio::test]
async fn dashboard_functions_are_admin_only_and_manage_jobs() {
    let options = AppOptions {
        dev: true,
        auth: AuthConfig::password().rate_limits(None),
        scheduler: SchedulerOptions::default().cron::<cron_tick>("dash", "0 0 0 * * *", ("d".to_string(),)),
        ..Default::default()
    };
    let t = TestApp::with_options(options).await.unwrap();
    let app = t.app().clone();
    let addr: SocketAddr = t.serve().await.unwrap();
    let http = Http { client: reqwest::Client::new(), base: format!("http://{addr}") };
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let (_, admin) = http
        .post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" }))
        .await;
    let (_, user) =
        http.post("/api/auth/signup", None, json!({ "email": "user@example.com", "password": "user password" })).await;
    let (admin, user) =
        (admin["accessToken"].as_str().unwrap().to_string(), user["accessToken"].as_str().unwrap().to_string());

    let pending = t.mutation::<add_and_schedule>(("p".to_string(), 3_600_000, false)).await.unwrap();
    let failed = t.mutation::<schedule_failing>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();

    let calls = [
        ("query", "jobs", page(None)),
        ("query", "crons", json!({})),
        ("mutation", "cancel_job", json!({ "id": pending })),
        ("mutation", "run_job_now", json!({ "id": pending })),
    ];
    for (kind, name, args) in &calls {
        let (s, b) = http.call(kind, name, args.clone(), None).await;
        assert_eq!((s, code_of(&b)), (400, "unauthenticated"), "anonymous {name}: {b}");
        let (s, b) = http.call(kind, name, args.clone(), Some(&user)).await;
        assert_eq!((s, code_of(&b)), (400, "forbidden"), "user {name}: {b}");
    }
    // The internal mutations can't be called by clients at all.
    let (s, _) = http.call("mutation", "schedule", json!({}), Some(&admin)).await;
    assert_eq!(s, 404);
    assert_eq!(job(&t, pending).await.state, JobState::Pending, "nothing above ran");

    // Listing, filtered by state.
    let all = http.ok("query", "jobs", page(None), &admin).await;
    assert_eq!(all["page"].as_array().unwrap().len(), 2);
    let only_failed = http.ok("query", "jobs", page(Some("failed")), &admin).await;
    let ids: Vec<_> = only_failed["page"].as_array().unwrap().iter().map(|j| j["_id"].clone()).collect();
    assert_eq!(ids, vec![json!(failed)]);
    assert_eq!(only_failed["page"][0]["state"], "failed");
    let crons = http.ok("query", "crons", json!({}), &admin).await;
    assert_eq!(crons[0]["name"], "dash");
    assert_eq!(crons[0]["schedule"], "0 0 0 * * *");
    assert!(crons[0]["nextRun"].as_f64().is_some() && crons[0]["lastRun"].is_null());

    // Retry a failed job: a new job with the same function and args.
    let retry = http.ok("mutation", "run_job_now", json!({ "id": failed }), &admin).await;
    let retry: Id<ScheduledJob> = retry.as_str().unwrap().parse().unwrap();
    assert_ne!(retry, failed);
    let retried = job(&t, retry).await;
    assert_eq!(
        (retried.state, retried.origin.as_deref(), retried.path.as_str()),
        (JobState::Pending, Some("dashboard"), failing_add::path().as_str())
    );
    // Run a pending job now: same id, due immediately.
    let same = http.ok("mutation", "run_job_now", json!({ "id": pending }), &admin).await;
    assert_eq!(same, json!(pending));
    assert_eq!(t.run_due_jobs().await.unwrap(), 2);
    assert_eq!(job(&t, pending).await.state, JobState::Success);

    // Cancel.
    let later = t.mutation::<add_and_schedule>(("l".to_string(), 3_600_000, false)).await.unwrap();
    let outcome = http.ok("mutation", "cancel_job", json!({ "id": later }), &admin).await;
    assert_eq!(serde_json::from_value::<CancelOutcome>(outcome).unwrap(), CancelOutcome::Canceled);
    let again = http.ok("mutation", "cancel_job", json!({ "id": later }), &admin).await;
    assert_eq!(again, "alreadyFinished");
    let (_, b) = http.call("mutation", "cancel_job", json!({ "id": "nonsense" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "invalid_id");
    let (_, b) = http.call("mutation", "run_job_now", json!({ "id": "nonsense" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "invalid_id");
}

#[tokio::test]
async fn subscribers_see_the_writes_of_scheduled_mutations() {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let t = TestApp::new().await.unwrap();
    t.start_scheduler().await.unwrap();
    let addr = t.serve().await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
    let session = format!("sched{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let send = |v: Value| Message::text(v.to_string());
    ws.send(send(json!({ "type": "Connect", "sessionId": session, "connectionCount": 0, "lastCloseReason": null, "clientVersion": "0.1.0" })))
        .await
        .unwrap();
    ws.send(send(json!({ "type": "ModifyQuerySet", "baseVersion": 0, "newVersion": 1,
        "modifications": [{ "type": "Add", "queryId": 0, "udfPath": notes::path(), "args": {} }] })))
        .await
        .unwrap();
    let mut next_values = async || -> Value {
        loop {
            let msg =
                tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("timed out").unwrap().unwrap();
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] == "Transition"
                && let Some(m) = v["modifications"].as_array().and_then(|m| m.first())
            {
                return m["value"].clone();
            }
        }
    };
    assert_eq!(next_values().await, json!([]));

    // The background runner picks the job up (woken by the commit) and the subscriber sees both writes.
    t.mutation::<add_and_schedule>(("live".to_string(), 50, false)).await.unwrap();
    let first = next_values().await;
    assert_eq!(first.as_array().unwrap().len(), 1, "{first}");
    let second = next_values().await;
    let texts: Vec<_> = second.as_array().unwrap().iter().map(|n| n["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(texts, vec!["live", "live (later)"]);
}

// ---------------------------------------------------------------------------------------------------------------------
// Canceling running actions
// ---------------------------------------------------------------------------------------------------------------------

/// Run the due jobs in the background (the test then interacts with the running ones).
fn run_in_background(t: &TestApp) -> tokio::task::JoinHandle<rxferrix::Result<usize>> {
    let app = t.app().clone();
    tokio::spawn(async move { app.run_due_jobs().await })
}

#[tokio::test]
async fn a_canceled_action_sees_it_and_stops_cooperatively() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<schedule_cooperative>(()).await.unwrap();
    let runner = run_in_background(&t);
    tokio::time::timeout(Duration::from_secs(5), COOP_STARTED.notified()).await.expect("action started");
    assert_eq!(job(&t, id).await.state, JobState::InProgress);

    // Canceled from a mutation: the job is `canceled` (with who did it) as soon as that commits.
    t.mutation::<cancel>((id,)).await.unwrap();
    let canceled = job(&t, id).await;
    assert_eq!(canceled.state, JobState::Canceled);
    assert!(canceled.completed_at.is_some());
    let reason = format!("Canceled by {} (while running)", cancel::path());
    assert!(canceled.error.as_deref().unwrap().starts_with(&reason), "{:?}", canceled.error);

    // The action sees it, cleans up and returns well within the grace period.
    let ran = tokio::time::timeout(Duration::from_secs(3), runner).await.expect("stopped before the grace period");
    assert_eq!(ran.unwrap().unwrap(), 1);
    let done = job(&t, id).await;
    assert_eq!((done.state, done.attempts), (JobState::Canceled, 1), "stays canceled: at most once");
    assert_eq!(done.result.as_deref(), Some("\"stopped\""));
    assert_eq!(done.error.as_deref(), Some(format!("{reason}; it returned before being aborted").as_str()));
    // Both of its mutations committed: the one from before the cancel and the cleanup.
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["coop: before", "coop: cleaned up"]);
    assert_eq!(t.run_due_jobs().await.unwrap(), 0);
}

#[tokio::test]
async fn an_uncooperative_action_is_aborted_after_the_grace_period() {
    let scheduler = SchedulerOptions { cancel_grace: Duration::from_millis(300), ..Default::default() };
    let t = TestApp::with_options(AppOptions { dev: true, scheduler, ..Default::default() }).await.unwrap();
    let id = t.mutation::<schedule_stubborn>(()).await.unwrap();
    assert_eq!(job(&t, id).await.max_attempts, 4);
    let runner = run_in_background(&t);
    tokio::time::timeout(Duration::from_secs(5), STUBBORN_STARTED.notified()).await.expect("action started");

    // Canceled from an action, with a reason.
    let started = std::time::Instant::now();
    t.action::<act_cancel>((id, "No longer needed".to_string())).await.unwrap();
    assert_eq!(job(&t, id).await.state, JobState::Canceled);
    // It ignores the cancel and starts a mutation that hangs while holding the writer; after the grace period the
    // action is dropped at its next await, which rolls that mutation back.
    let ran = tokio::time::timeout(Duration::from_secs(5), runner).await.expect("aborted");
    assert_eq!(ran.unwrap().unwrap(), 1);
    assert!(started.elapsed() >= Duration::from_millis(300), "not before the grace period");
    let done = job(&t, id).await;
    assert_eq!((done.state, done.attempts), (JobState::Canceled, 1));
    assert_eq!(done.error.as_deref(), Some("No longer needed (while running); aborted after the 300ms grace period"));
    assert!(done.result.is_none());
    // What it committed before the cancel stays; the mutation in flight at the abort was rolled back, and the
    // writer is free again.
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["stubborn: before"]);
    t.mutation::<add_note>(("after".to_string(),)).await.unwrap();
    // Never retried, even though it was scheduled with retries; a restart doesn't touch it either.
    assert_eq!(t.advance_time(Duration::from_secs(60)).await.unwrap(), 0);
    let t = t.restart().await.unwrap();
    assert_eq!(job(&t, id).await.state, JobState::Canceled);
    // The execution log has the aborted run.
    t.app().logs().flush().await.unwrap();
    let logs =
        t.app().logs().query(LogFilter { path_prefix: Some(stubborn::path()), ..Default::default() }).await.unwrap();
    let error = logs[0].error.as_deref().unwrap_or_default();
    assert!(error.contains("aborted 300ms after its job was canceled"), "{logs:?}");
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["stubborn: before", "after"]);
}

#[tokio::test]
async fn is_canceled_is_false_outside_scheduled_jobs() {
    // Called directly, an action is never canceled: `canceled()` never resolves.
    let t = TestApp::new().await.unwrap();
    let call = t.action::<wait_for_cancel>(());
    assert!(tokio::time::timeout(Duration::from_millis(200), call).await.is_err(), "still waiting");
}

// ---------------------------------------------------------------------------------------------------------------------
// Time zones, "run cron now" and system crons
// ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn crons_in_a_time_zone() {
    let options = SchedulerOptions::default()
        .cron_tz::<cron_tick>("nine", "0 0 9 * * *", "Europe/Bucharest", ("tz".to_string(),))
        .cron_tz::<cron_tick>("utc", "0 0 9 * * *", "UTC", ("utc".to_string(),));
    let t = TestApp::with_options(crons_options(options)).await.unwrap();
    let rows = cron_rows(&t).await;
    let nine = rows.iter().find(|r| r["name"] == "nine").unwrap();
    assert_eq!(nine["timezone"], "Europe/Bucharest");
    // 09:00 in Bucharest is 06:00 or 07:00 UTC (EEST / EET).
    let next = nine["nextRun"].as_f64().unwrap() as i64 / 1000;
    let hour_utc = next.rem_euclid(86_400) / 3600;
    assert!(next % 3600 == 0 && (hour_utc == 6 || hour_utc == 7), "{nine}");
    let utc = rows.iter().find(|r| r["name"] == "utc").unwrap();
    assert!(utc["timezone"].is_null(), "UTC is the default: {utc}");
    assert_eq!((utc["nextRun"].as_f64().unwrap() as i64 / 1000).rem_euclid(86_400), 9 * 3600);
    // It fires like any cron.
    assert_eq!(t.advance_time(Duration::from_secs(24 * 3600)).await.unwrap(), 2);

    // Changing only the zone recomputes the next run.
    let moved = SchedulerOptions::default().cron_tz::<cron_tick>(
        "nine",
        "0 0 9 * * *",
        "America/New_York",
        ("tz".to_string(),),
    );
    let t = t.restart_with(crons_options(moved)).await.unwrap();
    let nine = cron_rows(&t).await.remove(0);
    assert_eq!(nine["timezone"], "America/New_York");
    let hour_utc = (nine["nextRun"].as_f64().unwrap() as i64 / 1000).rem_euclid(86_400) / 3600;
    assert!(hour_utc == 13 || hour_utc == 14, "09:00 EDT/EST: {nine}");

    // Startup fails on an unknown zone, a zone on an interval, and app crons named like system crons.
    let bad =
        |s: SchedulerOptions| async move { TestApp::with_options(crons_options(s)).await.err().unwrap().to_string() };
    let msg =
        bad(SchedulerOptions::default().cron_tz::<cron_tick>("x", "0 0 9 * * *", "Mars/Olympus", ("a".to_string(),)))
            .await;
    assert!(msg.contains("unknown time zone `Mars/Olympus`"), "{msg}");
    let interval = rxferrix::CronSpec::interval::<cron_tick>("x", Duration::from_secs(60), ("a".to_string(),))
        .timezone("Europe/Paris");
    let msg = bad(SchedulerOptions { crons: vec![interval], ..Default::default() }).await;
    assert!(msg.contains("an interval has no time zone"), "{msg}");
    let msg =
        bad(SchedulerOptions::default().interval::<cron_tick>("_mine", Duration::from_secs(60), ("a".to_string(),)))
            .await;
    assert!(msg.contains("reserved"), "{msg}");
}

#[tokio::test]
async fn run_cron_now_and_cancel_running_actions_from_the_dashboard() {
    let options = AppOptions {
        dev: true,
        auth: AuthConfig::password().rate_limits(None),
        scheduler: SchedulerOptions::default()
            .cron::<cron_tick>("yearly", "0 0 0 1 1 *", ("now".to_string(),))
            .cron::<dash_hang>("hang", "0 0 0 1 1 *", ()),
        storage: StorageConfig::default().orphan_grace(Duration::ZERO),
        ..Default::default()
    };
    let t = TestApp::with_options(options).await.unwrap();
    let app = t.app().clone();
    let addr: SocketAddr = t.serve().await.unwrap();
    let http = Http { client: reqwest::Client::new(), base: format!("http://{addr}") };
    app.auth().create_admin(&app, "admin@example.com", "admin password".into()).await.unwrap();
    let (_, admin) = http
        .post("/api/auth/admin/signin", None, json!({ "email": "admin@example.com", "password": "admin password" }))
        .await;
    let (_, user) =
        http.post("/api/auth/signup", None, json!({ "email": "user@example.com", "password": "user password" })).await;
    let (admin, user) =
        (admin["accessToken"].as_str().unwrap().to_string(), user["accessToken"].as_str().unwrap().to_string());

    let (s, b) = http.call("mutation", "run_cron_now", json!({ "name": "yearly" }), None).await;
    assert_eq!((s, code_of(&b)), (400, "unauthenticated"));
    let (s, b) = http.call("mutation", "run_cron_now", json!({ "name": "yearly" }), Some(&user)).await;
    assert_eq!((s, code_of(&b)), (400, "forbidden"));
    let (_, b) = http.call("mutation", "run_cron_now", json!({ "name": "nope" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "not_found");

    // Enqueued now, as the cron's last run; its next run doesn't move.
    let before = http.ok("query", "crons", json!({}), &admin).await;
    let first: Id<ScheduledJob> = http
        .ok("mutation", "run_cron_now", json!({ "name": "yearly" }), &admin)
        .await
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let queued = job(&t, first).await;
    assert_eq!((queued.state, queued.origin.as_deref()), (JobState::Pending, Some("cron:yearly")));
    let crons = http.ok("query", "crons", json!({}), &admin).await;
    let yearly = crons.as_array().unwrap().iter().find(|c| c["name"] == "yearly").unwrap().clone();
    assert_eq!(yearly["lastJob"], json!(first));
    assert_eq!(yearly["lastJobState"], "pending");
    assert_eq!(
        yearly["nextRun"],
        before.as_array().unwrap().iter().find(|c| c["name"] == "yearly").unwrap()["nextRun"]
    );
    // No pile-up unless forced.
    let (_, b) = http.call("mutation", "run_cron_now", json!({ "name": "yearly" }), Some(&admin)).await;
    assert_eq!(code_of(&b), "running", "{b}");
    let forced = http.ok("mutation", "run_cron_now", json!({ "name": "yearly", "force": true }), &admin).await;
    assert_ne!(forced, json!(first));
    assert_eq!(t.run_due_jobs().await.unwrap(), 2);
    assert_eq!(texts(&t.query::<notes>(()).await.unwrap()), vec!["cron:now", "cron:now"]);

    // System crons are listed last, marked, and can be run now too.
    let crons = http.ok("query", "crons", json!({}), &admin).await;
    let names: Vec<_> = crons
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), c["system"].as_bool().unwrap()))
        .collect();
    assert_eq!(names, vec![("hang", false), ("yearly", false), ("_storage_sweep", true)]);
    assert_eq!(crons[2]["schedule"], "every 6h");
    let sweep: Id<ScheduledJob> = http
        .ok("mutation", "run_cron_now", json!({ "name": "_storage_sweep" }), &admin)
        .await
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 1);
    let swept = job(&t, sweep).await;
    assert_eq!(swept.state, JobState::Success, "{swept:?}");
    let stats: Value = serde_json::from_str(swept.result.as_deref().unwrap()).unwrap();
    assert_eq!(stats, json!({ "scanned": 0, "deleted": 0, "thumbnails": 0, "kept": 0 }));

    // The dashboard cancels a running action: it sees the cancel and stops.
    let hang: Id<ScheduledJob> =
        http.ok("mutation", "run_cron_now", json!({ "name": "hang" }), &admin).await.as_str().unwrap().parse().unwrap();
    let runner = run_in_background(&t);
    tokio::time::timeout(Duration::from_secs(5), DASH_STARTED.notified()).await.expect("action started");
    let outcome = http.ok("mutation", "cancel_job", json!({ "id": hang }), &admin).await;
    assert_eq!(serde_json::from_value::<CancelOutcome>(outcome).unwrap(), CancelOutcome::Canceled);
    assert_eq!(tokio::time::timeout(Duration::from_secs(3), runner).await.unwrap().unwrap().unwrap(), 1);
    let canceled = job(&t, hang).await;
    assert_eq!(canceled.state, JobState::Canceled);
    let error = canceled.error.clone().unwrap();
    assert!(
        error.starts_with("Canceled from the dashboard by admin:")
            && error.ends_with("(while running); it stopped with an error: gave up"),
        "{error}"
    );
    let again = http.ok("mutation", "cancel_job", json!({ "id": hang }), &admin).await;
    assert_eq!(again, "alreadyFinished");
}
