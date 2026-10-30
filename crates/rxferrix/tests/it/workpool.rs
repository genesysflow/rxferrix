//! Workpools (`rxferrix::scheduler::Workpool`): a pool's actions run at most `max_parallelism` at once, without holding
//! up other pools; pool defaults for retries, backoff and jitter; `on_complete` runs exactly once with the outcome (success,
//! failed for good, canceled), from mutations and actions; `cancel_pool`; and the background loop starts a pool's
//! next action as soon as a slot frees up.

use std::sync::atomic::Ordering;
use std::time::Duration;

use rxferrix::scheduler::{JobState, ScheduledJob, SchedulerOptions, Workpool};
use rxferrix::testing::TestApp;
use rxferrix::{AppOptions, Doc, Id};

mod app {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use rxferrix::prelude::*;
    use rxferrix::scheduler::{ScheduledJob, WorkResult};

    #[table(name = "pool_events")]
    pub struct Event {
        pub text: String,
    }

    #[query]
    pub async fn events(ctx: &QueryCtx) -> Result<Vec<String>> {
        Ok(ctx.db.query::<Event>().collect().await?.into_iter().map(|e| e.value.text).collect())
    }

    #[query]
    pub async fn job(ctx: &QueryCtx, id: Id<ScheduledJob>) -> Result<Option<Doc<ScheduledJob>>> {
        ctx.db.get(id).await
    }

    pub static RUNNING: AtomicUsize = AtomicUsize::new(0);
    pub static MOST: AtomicUsize = AtomicUsize::new(0);
    pub static ORDER: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Sleeps `ms`, tracking how many run at once; records when it starts and ends.
    #[internal_action]
    pub async fn work(_ctx: &ActionCtx, label: String, ms: u64) -> Result<String> {
        let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        MOST.fetch_max(now, Ordering::SeqCst);
        ORDER.lock().unwrap().push(format!("start {label}"));
        tokio::time::sleep(Duration::from_millis(ms)).await;
        ORDER.lock().unwrap().push(format!("end {label}"));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        Ok(label)
    }

    pub static FLAKY_RUNS: AtomicUsize = AtomicUsize::new(0);

    /// Fails its first `failures` runs.
    #[internal_action]
    pub async fn flaky(_ctx: &ActionCtx, failures: usize) -> Result<usize> {
        let run = FLAKY_RUNS.fetch_add(1, Ordering::SeqCst) + 1;
        if run <= failures {
            return Err(anyhow::anyhow!("failure {run}"));
        }
        Ok(run)
    }

    #[internal_mutation]
    pub async fn record(ctx: &MutationCtx, text: String) -> Result<String> {
        if text == "fail" {
            return Err(UserError::new("nope", "failing on purpose").into());
        }
        ctx.db.insert(Event { text: text.clone() }).await?;
        Ok(text.to_uppercase())
    }

    /// The `on_complete` of everything below: records `<context>: <outcome>`.
    #[internal_mutation]
    pub async fn done(ctx: &MutationCtx, work_id: Id<ScheduledJob>, context: String, result: WorkResult) -> Result<()> {
        let outcome = match result {
            WorkResult::Success { return_value } => format!("success {return_value}"),
            WorkResult::Failed { error } => format!("failed {error}"),
            WorkResult::Canceled => "canceled".to_string(),
        };
        assert!(ctx.db.get(work_id).await?.is_some_and(|j| j.state.is_finished()), "the work is finished first");
        ctx.db.insert(Event { text: format!("{context}: {outcome}") }).await?;
        Ok(())
    }

    /// A typed `on_complete`.
    #[internal_mutation]
    pub async fn done_typed(
        ctx: &MutationCtx,
        work_id: Id<ScheduledJob>,
        context: u32,
        result: WorkResult<String>,
    ) -> Result<()> {
        let _ = work_id;
        let text = format!("typed {context}: {}", result.into_result()?);
        ctx.db.insert(Event { text }).await?;
        Ok(())
    }

    #[mutation]
    pub async fn enqueue(ctx: &MutationCtx, pool: Option<String>, label: String, ms: u64) -> Result<Id<ScheduledJob>> {
        match pool {
            Some(pool) => ctx.scheduler.pool(&pool).enqueue::<work>((label, ms)).await,
            None => ctx.scheduler.run_after::<work>(Duration::ZERO, (label, ms)).await,
        }
    }

    /// Every kind of ending, each with `done` as `on_complete`.
    #[mutation]
    pub async fn endings(ctx: &MutationCtx) -> Result<Vec<Id<ScheduledJob>>> {
        let s = &ctx.scheduler;
        let ok = s.on_complete::<done>("mutation ok").enqueue::<record>(("hello".to_string(),)).await?;
        let failed = s.on_complete::<done>("mutation failed").enqueue::<record>(("fail".to_string(),)).await?;
        let action = s.pool("io").on_complete::<done>("action ok").enqueue::<work>(("a".to_string(), 1)).await?;
        let retried = s
            .pool("io")
            .with_retries(2)
            .backoff(Duration::ZERO, 1.0)
            .on_complete::<done>("action failed")
            .enqueue::<flaky>((10,))
            .await?;
        let canceled = s
            .on_complete::<done>("canceled")
            .run_after::<record>(Duration::from_secs(60), ("never".to_string(),))
            .await?;
        s.cancel(canceled).await?;
        let typed = s.on_complete::<done_typed>(7).enqueue::<work>(("t".to_string(), 1)).await?;
        Ok(vec![ok, failed, action, retried, canceled, typed])
    }

    /// The same from an action (through `_system/scheduler:schedule`).
    #[internal_action]
    pub async fn from_action(ctx: &ActionCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.pool("io").on_complete::<done>("from an action").enqueue::<work>(("x".to_string(), 1)).await
    }

    #[mutation]
    pub async fn with_pool_defaults(ctx: &MutationCtx, retries: Option<u32>) -> Result<Id<ScheduledJob>> {
        let with = ctx.scheduler.pool("retrying");
        let with = match retries {
            Some(n) => with.with_retries(n),
            None => with,
        };
        with.enqueue::<flaky>((0,)).await
    }

    /// `count` always-failing actions in pool `jittered`, with the pool's jitter or `jitter`.
    #[mutation]
    pub async fn enqueue_jittered(ctx: &MutationCtx, jitter: Option<f64>, count: u32) -> Result<Vec<Id<ScheduledJob>>> {
        let mut ids = vec![];
        for _ in 0..count {
            let with = ctx.scheduler.pool("jittered");
            let with = match jitter {
                Some(j) => with.jitter(j),
                None => with,
            };
            ids.push(with.enqueue::<flaky>((usize::MAX,)).await?);
        }
        Ok(ids)
    }

    #[mutation]
    pub async fn into_unknown_pool(ctx: &MutationCtx) -> Result<Id<ScheduledJob>> {
        ctx.scheduler.pool("nope").enqueue::<work>(("x".to_string(), 1)).await
    }

    #[mutation]
    pub async fn fill_and_cancel(ctx: &MutationCtx) -> Result<u64> {
        for i in 0..3 {
            ctx.scheduler
                .pool("io")
                .on_complete::<done>(format!("job {i}"))
                .enqueue::<work>((i.to_string(), 1))
                .await?;
        }
        ctx.scheduler.pool("other").enqueue::<work>(("kept".to_string(), 1)).await?;
        ctx.scheduler.cancel_pool("io").await
    }
}

use app::*;

fn options() -> AppOptions {
    let scheduler = SchedulerOptions::default()
        .workpool(Workpool::new("io").max_parallelism(2))
        .workpool(Workpool::new("one").max_parallelism(1))
        .workpool(Workpool::new("other"))
        .workpool(Workpool::new("retrying").retries(3).backoff(Duration::from_millis(250), 3.0))
        .workpool(Workpool::new("jittered").retries(1).backoff(Duration::from_secs(10), 1.0).jitter(1.0));
    AppOptions { scheduler, ..Default::default() }
}

async fn job(t: &TestApp, id: Id<ScheduledJob>) -> Doc<ScheduledJob> {
    t.query::<app::job>((id,)).await.unwrap().expect("job exists")
}

async fn events(t: &TestApp) -> Vec<String> {
    let mut events = t.query::<app::events>(()).await.unwrap();
    events.sort();
    events
}

/// The tests below share the statics: one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn reset() {
    RUNNING.store(0, Ordering::SeqCst);
    MOST.store(0, Ordering::SeqCst);
    FLAKY_RUNS.store(0, Ordering::SeqCst);
    ORDER.lock().unwrap().clear();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pool_runs_at_most_max_parallelism_actions_and_doesnt_hold_up_others() {
    let _serial = SERIAL.lock().await;
    reset();
    let t = TestApp::with_options(options()).await.unwrap();
    let mut ids = vec![];
    for i in 0..6 {
        ids.push(t.mutation::<enqueue>((Some("io".to_string()), format!("io{i}"), 40)).await.unwrap());
    }
    assert_eq!(t.run_due_jobs().await.unwrap(), 6);
    assert_eq!(MOST.load(Ordering::SeqCst), 2, "pool `io` allows 2 at a time");
    for id in &ids {
        assert_eq!(job(&t, *id).await.state, JobState::Success);
    }
    // Claimed in order within the pool (the claim commits `startedAt`; the spawned tasks themselves may begin in
    // any order).
    let mut started = vec![];
    for id in &ids {
        started.push(job(&t, *id).await.started_at.unwrap());
    }
    assert!(started.windows(2).all(|w| w[0] <= w[1]), "{started:?}");

    // A full pool doesn't hold up the default pool or another pool.
    reset();
    for i in 0..3 {
        t.mutation::<enqueue>((Some("one".to_string()), format!("slow{i}"), 150)).await.unwrap();
    }
    t.mutation::<enqueue>((None, "default".to_string(), 1)).await.unwrap();
    t.mutation::<enqueue>((Some("other".to_string()), "other".to_string(), 1)).await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 5);
    let order = ORDER.lock().unwrap().clone();
    let at = |e: &str| order.iter().position(|x| x == e).unwrap_or_else(|| panic!("{e} in {order:?}"));
    assert!(at("end default") < at("end slow0") && at("end other") < at("end slow0"), "{order:?}");
    assert!(at("end slow0") < at("start slow1") && at("end slow1") < at("start slow2"), "{order:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_complete_runs_exactly_once_with_how_the_work_ended() {
    let _serial = SERIAL.lock().await;
    reset();
    let t = TestApp::with_options(options()).await.unwrap();
    let ids = t.mutation::<endings>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    // The flaky action ran 3 times (2 retries) and its on_complete ran once, at the end.
    assert_eq!(FLAKY_RUNS.load(Ordering::SeqCst), 3);
    assert_eq!(job(&t, ids[3]).await.attempts, 3);
    assert_eq!(
        events(&t).await,
        vec![
            "action failed: failed failure 3",
            "action ok: success \"a\"",
            "canceled: canceled",
            "hello",
            "mutation failed: failed failing on purpose",
            "mutation ok: success \"HELLO\"",
            "typed 7: t",
        ]
    );
    // Each on_complete is a job of its own, named after the work.
    let on_complete = t.query::<app::job>((ids[0],)).await.unwrap().unwrap();
    assert_eq!(on_complete.on_complete.as_ref().unwrap().context, "mutation ok");
    assert_eq!(t.run_due_jobs().await.unwrap(), 0, "nothing runs twice");

    // From an action too.
    reset();
    t.action::<from_action>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    assert!(events(&t).await.contains(&"from an action: success \"x\"".to_string()));
}

#[tokio::test]
async fn pool_defaults_apply_unless_overridden_and_mistakes_are_errors() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let id = t.mutation::<with_pool_defaults>((None,)).await.unwrap();
    let j = job(&t, id).await;
    assert_eq!((j.pool.as_deref(), j.max_attempts), (Some("retrying"), 4));
    assert_eq!(j.backoff.map(|b| (b.initial_ms, b.base)), Some((250.0, 3.0)));
    let id = t.mutation::<with_pool_defaults>((Some(0),)).await.unwrap();
    assert_eq!(job(&t, id).await.max_attempts, 1);

    let err = t.mutation::<into_unknown_pool>(()).await.unwrap_err();
    assert!(format!("{err:#}").contains("no workpool is named `nope`"), "{err:#}");
    // (`on_complete::<an_action>` doesn't compile: it takes a `MutationFunction`.)

    // Bad declarations fail at startup.
    for (pools, msg) in [
        (vec![Workpool::new("a"), Workpool::new("a")], "two workpools are named `a`"),
        (vec![Workpool::new("a").max_parallelism(0)], "at least 1"),
        (vec![Workpool::new("")], "1 to 64 characters"),
        (vec![Workpool::new("a").jitter(1.5)], "the jitter must be between 0 and 1"),
    ] {
        let scheduler = SchedulerOptions { pools, ..Default::default() };
        let Err(err) = TestApp::with_options(AppOptions { scheduler, ..Default::default() }).await else {
            panic!("accepted: {msg}")
        };
        assert!(format!("{err:#}").contains(msg), "{err:#}");
    }
}

#[tokio::test]
async fn jitter_spreads_retries_over_the_backoff() {
    let _serial = SERIAL.lock().await;
    reset();
    let t = TestApp::with_options(options()).await.unwrap();
    // Pool `jittered`: a 10s backoff with full jitter, so each retry waits anywhere from 0 to 10s.
    let ids = t.mutation::<enqueue_jittered>((None, 20)).await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 20);
    let now = t.app().db().clock().now_ms();
    let mut delays = vec![];
    for id in &ids {
        let j = job(&t, *id).await;
        assert_eq!((j.state, j.jitter), (JobState::Pending, Some(1.0)), "retrying, with the pool's jitter");
        // The delay was added to a time between the start of the run and now.
        assert!(j.run_at - now <= 10_000.0 && j.run_at >= j.started_at.unwrap(), "{} within the backoff", j.run_at);
        delays.push(j.run_at - now);
    }
    let (min, max) = delays.iter().fold((f64::MAX, f64::MIN), |(lo, hi), d| (lo.min(*d), hi.max(*d)));
    assert!(max - min > 1000.0, "retries are spread out: {delays:?}");

    // A call's own jitter overrides the pool's: none, so the full 10s.
    let ids = t.mutation::<enqueue_jittered>((Some(0.0), 3)).await.unwrap();
    assert_eq!(t.run_due_jobs().await.unwrap(), 3);
    let now = t.app().db().clock().now_ms();
    for id in &ids {
        let j = job(&t, *id).await;
        assert_eq!(j.jitter, Some(0.0));
        assert!(j.run_at - now <= 10_000.0 && j.run_at - j.started_at.unwrap() >= 10_000.0, "{}", j.run_at);
    }

    let err = t.mutation::<enqueue_jittered>((Some(2.0), 1)).await.unwrap_err();
    assert!(format!("{err:#}").contains("the retry jitter must be between 0 and 1"), "{err:#}");
    let scheduler = SchedulerOptions { retry_jitter: -0.5, ..Default::default() };
    let Err(err) = TestApp::with_options(AppOptions { scheduler, ..Default::default() }).await else {
        panic!("accepted a negative retry_jitter")
    };
    assert!(format!("{err:#}").contains("the retry jitter must be between 0 and 1"), "{err:#}");
}

#[tokio::test]
async fn cancel_pool_cancels_only_that_pool_and_tells_on_complete() {
    let _serial = SERIAL.lock().await;
    reset();
    let t = TestApp::with_options(options()).await.unwrap();
    assert_eq!(t.mutation::<fill_and_cancel>(()).await.unwrap(), 3);
    t.run_due_jobs().await.unwrap();
    assert_eq!(events(&t).await, vec!["job 0: canceled", "job 1: canceled", "job 2: canceled"]);
    assert_eq!(ORDER.lock().unwrap().clone(), vec!["start kept", "end kept"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_background_loop_starts_the_next_action_when_a_slot_frees_up() {
    let _serial = SERIAL.lock().await;
    reset();
    let t = TestApp::with_options(options()).await.unwrap();
    let mut ids = vec![];
    for i in 0..4 {
        ids.push(t.mutation::<enqueue>((Some("one".to_string()), format!("bg{i}"), 30)).await.unwrap());
    }
    t.app().start_scheduler().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut done = 0;
        for id in &ids {
            if job(&t, *id).await.state == JobState::Success {
                done += 1;
            }
        }
        if done == ids.len() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "only {done} of {} finished", ids.len());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(MOST.load(Ordering::SeqCst), 1);
}
