//! Durable workflows (`rxferrix::workflow`): steps run in order and their outcomes are journaled; parallel steps;
//! failures the body handles or doesn't; retries and pools; durable sleep across a restart; events sent before or
//! after the wait; cancel; on_complete; starting from mutations (transactionally) and actions; long runs of query
//! steps; and the ways a body can break (nondeterminism, awaiting something else, panics).

use std::sync::atomic::Ordering;
use std::time::Duration;

use rxferrix::scheduler::{JobState, SchedulerOptions, Workpool};
use rxferrix::testing::TestApp;
use rxferrix::workflow::{StepKind, StepState, Workflow, WorkflowState};
use rxferrix::{AppOptions, Doc, FunctionRef, Id, Identity};
use serde_json::json;

mod app {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use rxferrix::prelude::*;
    use rxferrix::scheduler::{ScheduledJob, WorkResult};
    use rxferrix::workflow::{Workflow, WorkflowStep};

    #[table(name = "wf_log")]
    pub struct LogLine {
        pub text: String,
    }

    #[query]
    pub async fn log(ctx: &QueryCtx) -> Result<Vec<String>> {
        Ok(ctx.db.query::<LogLine>().collect().await?.into_iter().map(|l| l.value.text).collect())
    }

    #[query]
    pub async fn workflow(ctx: &QueryCtx, id: Id<Workflow>) -> Result<Option<Doc<Workflow>>> {
        ctx.db.get(id).await
    }

    #[internal_query]
    pub async fn steps(ctx: &QueryCtx, id: Id<Workflow>) -> Result<Vec<Doc<WorkflowStep>>> {
        ctx.db
            .query::<WorkflowStep>()
            .with_index(WorkflowStep::by_workflow, |q| q.eq(WorkflowStep::workflow, id))
            .collect()
            .await
    }

    #[query]
    pub async fn job(ctx: &QueryCtx, id: Id<ScheduledJob>) -> Result<Option<Doc<ScheduledJob>>> {
        ctx.db.get(id).await
    }

    #[internal_query]
    pub async fn count_log(ctx: &QueryCtx) -> Result<usize> {
        Ok(ctx.db.query::<LogLine>().collect().await?.len())
    }

    #[internal_mutation]
    pub async fn write(ctx: &MutationCtx, text: String) -> Result<String> {
        ctx.db.insert(LogLine { text: text.clone() }).await?;
        Ok(format!("wrote {text}"))
    }

    pub static ACTION_RUNS: AtomicUsize = AtomicUsize::new(0);

    #[internal_action]
    pub async fn shout(_ctx: &ActionCtx, text: String, fail: bool) -> Result<String> {
        ACTION_RUNS.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        if fail {
            return Err(anyhow::anyhow!("could not shout {text}"));
        }
        Ok(text.to_uppercase())
    }

    pub static FLAKY: AtomicUsize = AtomicUsize::new(0);

    #[internal_action]
    pub async fn flaky(_ctx: &ActionCtx) -> Result<usize> {
        let run = FLAKY.fetch_add(1, Ordering::SeqCst) + 1;
        if run < 3 {
            return Err(anyhow::anyhow!("flaky failure {run}"));
        }
        Ok(run)
    }

    /// Query → mutation → action → mutation, each using the previous result.
    #[workflow]
    pub async fn pipeline(step: &WorkflowCtx, name: String) -> Result<String> {
        let before = step.run_query::<count_log>(()).await?;
        let wrote = step.run_mutation::<write>((format!("{name} {before}"),)).await?;
        let loud = step.run_action::<shout>((wrote, false)).await?;
        step.run_mutation::<write>((loud.clone(),)).await?;
        Ok(loud)
    }

    #[mutation]
    pub async fn start_pipeline(ctx: &MutationCtx, name: String, fail: bool) -> Result<Id<Workflow>> {
        let id = ctx.workflows.on_complete::<finished>(name.clone()).start::<pipeline>((name,)).await?;
        if fail {
            return Err(UserError::new("nope", "rolled back").into());
        }
        Ok(id)
    }

    /// on_complete of workflows: logs `done <context>: <outcome>`.
    #[internal_mutation]
    pub async fn finished(
        ctx: &MutationCtx,
        workflow_id: Id<Workflow>,
        context: String,
        result: WorkResult,
    ) -> Result<()> {
        let outcome = match result {
            WorkResult::Success { return_value } => format!("success {return_value}"),
            WorkResult::Failed { error } => format!("failed {error}"),
            WorkResult::Canceled => "canceled".into(),
        };
        assert!(ctx.db.get(workflow_id).await?.is_some_and(|w| w.state.is_finished()));
        ctx.db.insert(LogLine { text: format!("done {context}: {outcome}") }).await?;
        Ok(())
    }

    /// Two actions at once, then a mutation with both results.
    #[workflow]
    pub async fn fan_out(step: &WorkflowCtx) -> Result<Vec<String>> {
        let (a, b) = futures::try_join!(
            step.run_action::<shout>(("a".to_string(), false)).into_future(),
            step.run_action::<shout>(("b".to_string(), false)).into_future(),
        )?;
        step.run_mutation::<write>((format!("{a}{b}"),)).await?;
        Ok(vec![a, b])
    }

    /// Handles one failure (compensates), retries a flaky action, then fails for real if `give_up`.
    #[workflow]
    pub async fn careful(step: &WorkflowCtx, give_up: bool) -> Result<usize> {
        if let Err(e) = step.run_action::<shout>(("x".to_string(), true)).await {
            step.run_mutation::<write>((format!("compensated: {e}"),)).await?;
        }
        let run = step.run_action::<flaky>(()).with_retries(3).backoff(Duration::ZERO, 1.0).pool("slow").await?;
        if give_up {
            step.run_action::<shout>(("y".to_string(), true)).await?;
        }
        Ok(run)
    }

    /// Sleeps an hour between two writes.
    #[workflow]
    pub async fn napper(step: &WorkflowCtx) -> Result<()> {
        step.run_mutation::<write>(("before nap".to_string(),)).await?;
        step.sleep(Duration::from_secs(3600)).await?;
        step.run_mutation::<write>(("after nap".to_string(),)).await?;
        Ok(())
    }

    /// Waits for two events.
    #[workflow]
    pub async fn approval(step: &WorkflowCtx) -> Result<String> {
        let who: String = step.wait_for_event("approved").await?;
        let note: String = step.wait_for_event("note").await?;
        step.run_mutation::<write>((format!("{who}: {note}"),)).await?;
        Ok(who)
    }

    #[workflow]
    pub async fn many_queries(step: &WorkflowCtx, n: usize) -> Result<usize> {
        let mut total = 0;
        for _ in 0..n {
            total += step.run_query::<count_log>(()).await? + 1;
        }
        Ok(total)
    }

    pub static FLIP: AtomicBool = AtomicBool::new(false);

    /// Starts a different step once `FLIP` is set: not deterministic.
    #[workflow]
    pub async fn fickle(step: &WorkflowCtx) -> Result<()> {
        if FLIP.load(Ordering::SeqCst) {
            step.run_action::<shout>(("x".to_string(), false)).await?;
        } else {
            step.run_mutation::<write>(("x".to_string(),)).await?;
        }
        step.sleep(Duration::from_secs(60)).await?;
        Ok(())
    }

    #[workflow]
    pub async fn awaits_something_else(_step: &WorkflowCtx) -> Result<()> {
        std::future::pending::<()>().await;
        Ok(())
    }

    #[workflow]
    pub async fn panics(step: &WorkflowCtx) -> Result<()> {
        step.run_query::<count_log>(()).await?;
        panic!("boom");
    }

    #[workflow]
    pub async fn into_unknown_pool(step: &WorkflowCtx) -> Result<String> {
        match step.run_action::<shout>(("x".to_string(), false)).pool("nope").await {
            Ok(v) => Ok(v),
            Err(e) => Ok(format!("handled: {e}")),
        }
    }

    #[mutation]
    pub async fn start_napper(ctx: &MutationCtx) -> Result<Id<Workflow>> {
        ctx.workflows.on_complete::<finished>("napper").start::<napper>(()).await
    }

    #[mutation]
    pub async fn start_fan_out(ctx: &MutationCtx) -> Result<Id<Workflow>> {
        ctx.workflows.start::<fan_out>(()).await
    }

    #[mutation]
    pub async fn start_careful(ctx: &MutationCtx, give_up: bool) -> Result<Id<Workflow>> {
        ctx.workflows.on_complete::<finished>("careful").start::<careful>((give_up,)).await
    }

    #[mutation]
    pub async fn start_approval(ctx: &MutationCtx) -> Result<Id<Workflow>> {
        ctx.workflows.start::<approval>(()).await
    }

    #[mutation]
    pub async fn start_many_queries(ctx: &MutationCtx, n: usize) -> Result<Id<Workflow>> {
        ctx.workflows.start::<many_queries>((n,)).await
    }

    #[mutation]
    pub async fn start_broken(ctx: &MutationCtx, which: String) -> Result<Id<Workflow>> {
        match which.as_str() {
            "fickle" => ctx.workflows.start::<fickle>(()).await,
            "awaits" => ctx.workflows.start::<awaits_something_else>(()).await,
            "panics" => ctx.workflows.start::<panics>(()).await,
            _ => ctx.workflows.start::<into_unknown_pool>(()).await,
        }
    }

    /// A query that outlasts the test's mutation timeout (query steps run inside the engine's `advance` mutation).
    #[query]
    pub async fn slow_count(ctx: &QueryCtx) -> Result<usize> {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(ctx.db.query::<LogLine>().collect().await?.len())
    }

    #[workflow]
    pub async fn stalls_the_engine(step: &WorkflowCtx) -> Result<usize> {
        step.run_query::<slow_count>(()).await
    }

    #[mutation]
    pub async fn start_stalling(ctx: &MutationCtx) -> Result<Id<Workflow>> {
        ctx.workflows.start::<stalls_the_engine>(()).await
    }

    #[mutation]
    pub async fn send(ctx: &MutationCtx, id: Id<Workflow>, name: String, value: String) -> Result<()> {
        ctx.workflows.send_event(id, &name, value).await
    }

    #[mutation]
    pub async fn cancel(ctx: &MutationCtx, id: Id<Workflow>) -> Result<()> {
        ctx.workflows.cancel(id).await
    }

    /// Starts `approval` and sends both events, from an action.
    #[internal_action]
    pub async fn from_action(ctx: &ActionCtx) -> Result<Id<Workflow>> {
        let id = ctx.workflows.start::<approval>(()).await?;
        ctx.workflows.send_event(id, "approved", "ana").await?;
        ctx.workflows.send_event(id, "note", "from an action").await?;
        Ok(id)
    }
}

use app::*;

fn options() -> AppOptions {
    AppOptions {
        scheduler: SchedulerOptions::default().workpool(Workpool::new("slow").max_parallelism(1)),
        ..Default::default()
    }
}

async fn workflow(t: &TestApp, id: Id<Workflow>) -> Doc<Workflow> {
    t.query::<app::workflow>((id,)).await.unwrap().expect("workflow exists")
}

async fn steps(t: &TestApp, id: Id<Workflow>) -> Vec<(StepKind, String, StepState)> {
    let steps = t.query::<app::steps>((id,)).await.unwrap();
    steps.iter().map(|s| (s.kind, s.name.clone(), s.state)).collect()
}

async fn log(t: &TestApp) -> Vec<String> {
    t.query::<app::log>(()).await.unwrap()
}

/// The tests share statics: one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steps_run_in_order_and_the_result_reaches_on_complete() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    // A mutation that rolls back starts nothing.
    assert!(t.mutation::<start_pipeline>(("no".to_string(), true)).await.is_err());
    let id = t.mutation::<start_pipeline>(("hi".to_string(), false)).await.unwrap();
    assert_eq!(workflow(&t, id).await.state, WorkflowState::Running);
    t.run_due_jobs().await.unwrap();

    let wf = workflow(&t, id).await;
    assert_eq!(
        (wf.state, wf.result.clone(), wf.steps),
        (WorkflowState::Success, Some(json!("WROTE HI 0")), 4),
        "{wf:?}"
    );
    assert_eq!(
        steps(&t, id).await,
        vec![
            (StepKind::Query, count_log::path(), StepState::Success),
            (StepKind::Mutation, write::path(), StepState::Success),
            (StepKind::Action, shout::path(), StepState::Success),
            (StepKind::Mutation, write::path(), StepState::Success),
        ]
    );
    assert_eq!(log(&t).await, vec!["hi 0", "WROTE HI 0", "done hi: success \"WROTE HI 0\""]);
    // The steps' jobs are ordinary jobs, named after the workflow.
    let step_docs = t.query::<app::steps>((id,)).await.unwrap();
    let job = t.query::<app::job>((step_docs[2].job.unwrap(),)).await.unwrap().unwrap();
    assert_eq!((job.state, job.origin.clone()), (JobState::Success, Some(format!("workflow:{id}"))));
    assert_eq!(t.run_due_jobs().await.unwrap(), 0, "nothing runs twice");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_steps_start_together() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let id = t.mutation::<start_fan_out>(()).await.unwrap();
    // One round: the advance job starts both actions before either has run.
    let advance = t.app().run_due_jobs();
    advance.await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!((wf.state, wf.result.clone()), (WorkflowState::Success, Some(json!(["A", "B"]))));
    let step_docs = t.query::<app::steps>((id,)).await.unwrap();
    assert_eq!(step_docs[0].started_at, step_docs[1].started_at, "both started in the same transaction");
    assert_eq!(log(&t).await, vec!["AB"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_are_handled_retried_or_end_the_workflow() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    FLAKY.store(0, Ordering::SeqCst);
    let id = t.mutation::<start_careful>((false,)).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!((wf.state, wf.result.clone()), (WorkflowState::Success, Some(json!(3))), "{wf:?}");
    let step_docs = t.query::<app::steps>((id,)).await.unwrap();
    let flaky_job = t.query::<app::job>((step_docs[2].job.unwrap(),)).await.unwrap().unwrap();
    assert_eq!((flaky_job.attempts, flaky_job.pool.as_deref()), (3, Some("slow")));

    FLAKY.store(0, Ordering::SeqCst);
    let id = t.mutation::<start_careful>((true,)).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!(wf.state, WorkflowState::Failed);
    assert_eq!(wf.error.as_deref(), Some("could not shout y"));
    let lines = log(&t).await;
    assert!(lines.contains(&"compensated: could not shout x".to_string()), "{lines:?}");
    assert!(lines.contains(&"done careful: failed could not shout y".to_string()), "{lines:?}");
}

#[tokio::test]
async fn a_sleep_survives_a_restart_and_a_canceled_workflow_stops() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let id = t.mutation::<start_napper>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    assert_eq!(log(&t).await, vec!["before nap"]);
    let t = t.restart().await.unwrap();
    t.advance_time(Duration::from_secs(1800)).await.unwrap();
    assert_eq!(log(&t).await, vec!["before nap"], "half an hour in");
    t.advance_time(Duration::from_secs(1801)).await.unwrap();
    assert_eq!(log(&t).await, vec!["before nap", "after nap", "done napper: success null"]);
    assert_eq!(workflow(&t, id).await.state, WorkflowState::Success);

    // Canceled while it sleeps: the sleep's job is canceled too, and on_complete hears about it.
    let id = t.mutation::<start_napper>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    t.mutation::<cancel>((id,)).await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!(wf.state, WorkflowState::Canceled);
    assert!(wf.error.as_deref().unwrap().starts_with("Canceled by"), "{wf:?}");
    let step_docs = t.query::<app::steps>((id,)).await.unwrap();
    assert_eq!(step_docs[1].state, StepState::Canceled);
    let sleep_job = t.query::<app::job>((step_docs[1].job.unwrap(),)).await.unwrap().unwrap();
    assert_eq!(sleep_job.state, JobState::Canceled);
    t.advance_time(Duration::from_secs(7200)).await.unwrap();
    let lines = log(&t).await;
    assert_eq!(lines.iter().filter(|l| *l == "after nap").count(), 1, "{lines:?}");
    assert!(lines.contains(&"done napper: canceled".to_string()), "{lines:?}");
    t.mutation::<cancel>((id,)).await.unwrap(); // already finished: nothing happens
}

#[tokio::test]
async fn events_reach_the_workflow_whether_sent_before_or_after_it_waits() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let id = t.mutation::<start_approval>(()).await.unwrap();
    // "note" before the workflow waits for it (it isn't even running yet), "approved" while it waits.
    t.mutation::<send>((id, "note".to_string(), "looks good".to_string())).await.unwrap();
    t.run_due_jobs().await.unwrap();
    assert_eq!(steps(&t, id).await, vec![(StepKind::Event, "approved".to_string(), StepState::Running)]);
    t.mutation::<send>((id, "approved".to_string(), "ana".to_string())).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!((wf.state, wf.result.clone()), (WorkflowState::Success, Some(json!("ana"))));
    assert!(wf.events.is_empty());
    assert_eq!(log(&t).await, vec!["ana: looks good"]);
    let err = t.mutation::<send>((id, "note".to_string(), "late".to_string())).await.unwrap_err();
    assert!(format!("{err:#}").contains("has finished"), "{err:#}");

    // From an action.
    let id = t.action::<from_action>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    assert_eq!(workflow(&t, id).await.state, WorkflowState::Success);
    assert!(log(&t).await.contains(&"ana: from an action".to_string()));
}

#[tokio::test]
async fn long_runs_of_query_steps_continue_in_new_transactions() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let id = t.mutation::<start_many_queries>((250,)).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!((wf.state, wf.result.clone(), wf.steps), (WorkflowState::Success, Some(json!(250)), 250), "{wf:?}");
}

#[tokio::test]
async fn broken_bodies_fail_their_workflow() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();

    FLIP.store(false, Ordering::SeqCst);
    let fickle = t.mutation::<start_broken>(("fickle".to_string(),)).await.unwrap();
    t.run_due_jobs().await.unwrap();
    FLIP.store(true, Ordering::SeqCst); // the code "changed" while it slept
    t.advance_time(Duration::from_secs(61)).await.unwrap();
    let wf = workflow(&t, fickle).await;
    assert_eq!(wf.state, WorkflowState::Failed);
    assert!(wf.error.as_deref().unwrap().contains("not deterministic"), "{wf:?}");
    FLIP.store(false, Ordering::SeqCst);

    for (which, msg) in
        [("awaits", "waiting for something other than its steps"), ("panics", "the workflow panicked: boom")]
    {
        let id = t.mutation::<start_broken>((which.to_string(),)).await.unwrap();
        t.run_due_jobs().await.unwrap();
        let wf = workflow(&t, id).await;
        assert_eq!(wf.state, WorkflowState::Failed);
        assert!(wf.error.as_deref().unwrap().contains(msg), "{which}: {wf:?}");
    }

    // A step that can't even be scheduled fails, and the body can handle that.
    let id = t.mutation::<start_broken>(("pool".to_string(),)).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let wf = workflow(&t, id).await;
    let result = wf.result.clone().unwrap();
    assert!(result.as_str().unwrap().starts_with("handled: no workpool is named `nope`"), "{wf:?}");
}

#[tokio::test]
async fn a_failing_engine_job_fails_its_workflow() {
    let _serial = SERIAL.lock().await;
    let t =
        TestApp::with_options(AppOptions { mutation_timeout: Duration::from_millis(100), ..options() }).await.unwrap();
    let id = t.mutation::<start_stalling>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let wf = workflow(&t, id).await;
    assert_eq!(wf.state, WorkflowState::Failed, "not left running: {wf:?}");
    let error = wf.error.as_deref().unwrap();
    assert!(error.contains("the workflow engine's `_system/workflow:advance` job failed"), "{error}");
}

#[tokio::test]
async fn the_dashboard_lists_and_cancels_workflows_and_old_ones_are_cleaned_up() {
    let _serial = SERIAL.lock().await;
    let t = TestApp::with_options(options()).await.unwrap();
    let id = t.mutation::<start_approval>(()).await.unwrap();
    t.run_due_jobs().await.unwrap();
    let admin = t.app().auth().create_admin(t.app(), "wf-admin@example.com", "admin password".into()).await.unwrap();
    let admin = Identity::admin(admin, None);
    let page = json!({ "paginationOpts": { "numItems": 10, "cursor": null }, "state": "running" });
    let listed = t.app().run_query("_system/workflow:list", page.clone(), &admin).await.result.unwrap();
    assert_eq!(listed["page"][0]["_id"], json!(id.to_string()), "{listed}");
    let steps =
        t.app().run_query("_system/workflow:steps", json!({ "id": id.to_string() }), &admin).await.result.unwrap();
    assert_eq!(steps[0]["name"], "approved");
    assert!(t.app().run_query("_system/workflow:list", page, &Identity::anonymous()).await.result.is_err());
    let (outcome, _) = t
        .app()
        .run_mutation("_system/workflow:cancel_workflow", json!({ "id": id.to_string() }), &admin)
        .await
        .unwrap();
    assert_eq!(outcome, json!("canceled"));
    let wf = workflow(&t, id).await;
    assert!(wf.error.as_deref().unwrap().starts_with("Canceled from the dashboard by admin:"), "{wf:?}");

    // Past the retention, finished workflows go, with their steps.
    t.advance_time(Duration::from_secs(8 * 86_400)).await.unwrap();
    t.app().cleanup_scheduled_jobs().await.unwrap();
    assert!(t.query::<app::workflow>((id,)).await.unwrap().is_none());
    assert!(t.query::<app::steps>((id,)).await.unwrap().is_empty());
}
