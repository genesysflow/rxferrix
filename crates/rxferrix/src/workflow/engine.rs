//! Advancing a workflow: run its body against the journal, then start the steps it is waiting for, or record how it
//! ended. Everything here runs inside one mutation's transaction (`_system/workflow:advance`, `step_done`, `wake`),
//! so a step's outcome, the next steps and the jobs that run them commit together.
//!
//! - **Queries** run right there, in the same transaction, and the body is run again with their result (up to
//!   [`MAX_ROUNDS`] times; then the workflow continues in a new `advance` job).
//! - **Mutations and actions** become scheduled jobs (with their pool, retries and backoff) whose `on_complete` is
//!   `_system/workflow:step_done`: their outcome goes into the journal and the workflow advances, exactly once.
//! - **Sleeps** schedule `_system/workflow:wake` for when they end. **Events** wait in the journal for
//!   `send_event` (or take one it sent earlier).

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::Value;

use super::ctx::{NewStep, Replay, WorkflowCtx};
use super::tables::{StepKind, StepState, Workflow, WorkflowState, WorkflowStep};
use crate::UserError;
use crate::db::query::MutationDb;
use crate::id::Id;
use crate::private::{Handler, WorkflowDef};
use crate::runtime::{MutationCtx, QueryCtx};
use crate::scheduler::Scheduler;
use crate::scheduler::store::{self, WorkResult};
use crate::schema::Doc;

/// A workflow may start at most this many steps (its body is replayed in full every time it advances).
pub const MAX_STEPS: u32 = 10_000;
/// Runs of the body in one transaction (each after queries resolved inline) before continuing in a new one.
pub const MAX_ROUNDS: usize = 100;

/// How a workflow ended.
pub(crate) enum Ending {
    Success(Value),
    Failed(String),
    Canceled(String),
}

/// What one run of the body found.
enum Run {
    Done(Result<Value, String>),
    Waiting { new_steps: Vec<NewStep>, running: bool },
}

/// Run the body once, against `steps`, until it returns or can't go further. Synchronous: the body only awaits steps,
/// which never need a wake-up.
fn run_body(def: &WorkflowDef, id: Id<Workflow>, args: Value, steps: Vec<Doc<WorkflowStep>>) -> Run {
    let replay = Arc::new(Mutex::new(Replay::new(steps)));
    let mut body = (def.handler)(WorkflowCtx::new(replay.clone(), id), args);
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    let polled = std::panic::catch_unwind(AssertUnwindSafe(|| body.as_mut().poll(&mut cx)));
    drop(body);
    let mut replay = replay.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(fatal) = replay.fatal.take() {
        return Run::Done(Err(fatal));
    }
    match polled {
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Run::Done(Err(format!("the workflow panicked: {msg}")))
        }
        Ok(Poll::Ready(result)) => Run::Done(result.map_err(|e| format!("{e:#}"))),
        Ok(Poll::Pending) => {
            let running = replay.running().next().is_some();
            Run::Waiting { new_steps: std::mem::take(&mut replay.new_steps), running }
        }
    }
}

async fn steps_of(db: &MutationDb, id: Id<Workflow>) -> crate::Result<Vec<Doc<WorkflowStep>>> {
    db.query::<WorkflowStep>()
        .with_index(WorkflowStep::by_workflow, |q| q.eq(WorkflowStep::workflow, id))
        .collect()
        .await
}

/// Schedule `_system/workflow:advance` for `id`, due now.
pub(crate) async fn schedule_advance(scheduler: &Scheduler, id: Id<Workflow>) -> crate::Result<()> {
    scheduler.with_origin(format!("workflow:{id}")).run_after::<super::advance>(Duration::ZERO, (id,)).await?;
    Ok(())
}

/// Run the workflow's body and act on what it found (see the module docs). Does nothing for a finished workflow.
pub(crate) async fn advance(ctx: &MutationCtx, id: Id<Workflow>) -> crate::Result<()> {
    for _ in 0..MAX_ROUNDS {
        let Some(mut wf) = ctx.db.get(id).await? else { return Ok(()) };
        if wf.state.is_finished() {
            return Ok(());
        }
        let Some(def) = super::registry()?.get(wf.name.as_str()) else {
            let error = format!("no workflow is registered at `{}` any more", wf.name);
            return finish(&ctx.db, ctx.now(), wf, Ending::Failed(error)).await;
        };
        let (new_steps, running) = match run_body(def, id, wf.args.clone(), steps_of(&ctx.db, id).await?) {
            Run::Done(Ok(value)) => return finish(&ctx.db, ctx.now(), wf, Ending::Success(value)).await,
            Run::Done(Err(error)) => return finish(&ctx.db, ctx.now(), wf, Ending::Failed(error)).await,
            Run::Waiting { new_steps, running } => (new_steps, running),
        };
        if new_steps.is_empty() {
            if running {
                return Ok(());
            }
            let error = "the workflow is waiting for something other than its steps: its body may only await \
                         `step.run_query`, `run_mutation`, `run_action`, `sleep` and `wait_for_event`";
            return finish(&ctx.db, ctx.now(), wf, Ending::Failed(error.to_string())).await;
        }
        let last = new_steps.iter().map(|s| s.index + 1).max().unwrap_or(0);
        if last > MAX_STEPS {
            let error = format!("the workflow started more than {MAX_STEPS} steps");
            return finish(&ctx.db, ctx.now(), wf, Ending::Failed(error)).await;
        }
        let mut resolved = false;
        for step in new_steps {
            resolved |= start_step(ctx, &mut wf, step).await?;
        }
        wf.value.steps = wf.value.steps.max(last);
        ctx.db.replace(id, wf.value).await?;
        if !resolved {
            return Ok(());
        }
    }
    // Many queries in a row: go on in a transaction of its own.
    schedule_advance(&ctx.scheduler, id).await
}

/// Record a new step and start it. Returns true if it already has an outcome (a query, an event sent earlier, a
/// step that couldn't be scheduled): the body can be run again right away.
async fn start_step(ctx: &MutationCtx, wf: &mut Doc<Workflow>, step: NewStep) -> crate::Result<bool> {
    let now = ctx.now();
    let call = step.call;
    let mut doc = WorkflowStep {
        workflow: wf.id,
        index: step.index,
        kind: call.kind,
        name: call.name.clone(),
        args: call.args.clone(),
        state: StepState::Running,
        result: None,
        error: None,
        job: None,
        started_at: now,
        completed_at: None,
    };
    let scheduled = match call.kind {
        StepKind::Query => {
            match run_query(ctx, &call.name, call.args).await {
                Ok(value) => {
                    doc.state = StepState::Success;
                    doc.result = Some(value);
                }
                Err(error) => {
                    doc.state = StepState::Failed;
                    doc.error = Some(error);
                }
            }
            doc.completed_at = Some(now);
            ctx.db.insert(doc).await?;
            return Ok(true);
        }
        StepKind::Event => {
            if let Some(i) = wf.events.iter().position(|e| e.name == call.name) {
                let event = wf.value.events.remove(i);
                doc.state = StepState::Success;
                doc.result = Some(event.value);
                doc.completed_at = Some(now);
                ctx.db.insert(doc).await?;
                return Ok(true);
            }
            ctx.db.insert(doc).await?;
            return Ok(false);
        }
        StepKind::Mutation | StepKind::Action => {
            let step_id = ctx.db.insert(doc.clone()).await?;
            let scheduler = ctx.scheduler.with_origin(format!("workflow:{}", wf.id));
            let mut with = scheduler.on_complete::<super::step_done>(step_id);
            if let Some(pool) = &call.pool {
                with = with.pool(pool);
            }
            if let Some(retries) = call.retries {
                with = with.with_retries(retries);
            }
            if let Some(b) = call.backoff {
                with = with.backoff(Duration::from_secs_f64(b.initial_ms / 1000.0), b.base);
            }
            (step_id, with.run_path(call.name, call.args, Duration::ZERO).await)
        }
        StepKind::Sleep => {
            let step_id = ctx.db.insert(doc.clone()).await?;
            let ms = call.args["ms"].as_f64().unwrap_or(0.0).max(0.0);
            let scheduler = ctx.scheduler.with_origin(format!("workflow:{}", wf.id));
            (step_id, scheduler.run_after::<super::wake>(Duration::from_secs_f64(ms / 1000.0), (step_id,)).await)
        }
    };
    let (step_id, job) = scheduled;
    match job {
        Ok(job) => {
            doc.job = Some(job);
            ctx.db.replace(step_id, doc).await?;
            Ok(false)
        }
        // E.g. an undeclared pool: the step fails, and the body decides what to do about it.
        Err(e) => {
            doc.state = StepState::Failed;
            doc.error = Some(format!("{e:#}"));
            doc.completed_at = Some(now);
            ctx.db.replace(step_id, doc).await?;
            Ok(true)
        }
    }
}

/// Run a query step inside the transaction, with no identity.
async fn run_query(ctx: &MutationCtx, path: &str, args: Value) -> Result<Value, String> {
    let def = crate::runtime::registry().map_err(|e| format!("{e:#}"))?.get(path);
    let Some(Handler::Query(handler)) = def.map(|d| d.handler) else {
        return Err(format!("no query is registered at `{path}`"));
    };
    let storage = crate::storage::QueryStorage::new(ctx.db.share(), ctx.storage.service().clone());
    let qctx = QueryCtx::internal(ctx.db.share(), storage, ctx.env.secret_box(), ctx.is_dev());
    crate::runtime::catch_panics(handler(qctx, args)).await.map_err(|e| format!("{e:#}"))
}

/// A step's job (or sleep) has ended: record its outcome and advance the workflow. Ignored if the step isn't running
/// (its workflow was canceled or finished first).
pub(crate) async fn step_done(ctx: &MutationCtx, step_id: Id<WorkflowStep>, result: WorkResult) -> crate::Result<()> {
    let Some(step) = ctx.db.get(step_id).await? else { return Ok(()) };
    if step.state != StepState::Running {
        return Ok(());
    }
    let workflow = step.workflow;
    let mut value = step.value;
    match result {
        WorkResult::Success { return_value } => {
            value.state = StepState::Success;
            value.result = Some(return_value);
        }
        WorkResult::Failed { error } => {
            value.state = StepState::Failed;
            value.error = Some(error);
        }
        WorkResult::Canceled => {
            value.state = StepState::Canceled;
            value.error = Some("the step's job was canceled".to_string());
        }
    }
    value.completed_at = Some(ctx.now());
    ctx.db.replace(step_id, value).await?;
    advance(ctx, workflow).await
}

/// Record how the workflow ended, cancel its running steps, and schedule its `on_complete`.
pub(crate) async fn finish(db: &MutationDb, now: f64, wf: Doc<Workflow>, ending: Ending) -> crate::Result<()> {
    let id = wf.id;
    let mut value = wf.value;
    let (state, result) = match ending {
        Ending::Success(v) => {
            value.result = Some(v.clone());
            (WorkflowState::Success, WorkResult::Success { return_value: v })
        }
        Ending::Failed(error) => {
            value.error = Some(store::truncate(error.clone()));
            (WorkflowState::Failed, WorkResult::Failed { error })
        }
        Ending::Canceled(reason) => {
            value.error = Some(store::truncate(reason));
            (WorkflowState::Canceled, WorkResult::Canceled)
        }
    };
    let why = match state {
        WorkflowState::Canceled => "the workflow was canceled",
        _ => "the workflow ended first",
    };
    for step in steps_of(db, id).await? {
        if step.state != StepState::Running {
            continue;
        }
        let mut s = step.value;
        if let Some(job) = s.job {
            store::cancel(db, job, now, &format!("Canceled because {why}")).await?;
        }
        s.state = StepState::Canceled;
        s.error = Some(why.to_string());
        s.completed_at = Some(now);
        db.replace(step.id, s).await?;
    }
    value.state = state;
    value.completed_at = Some(now);
    if let Some(on_complete) = &value.on_complete {
        let origin = format!("workflow:{id}");
        store::schedule_on_complete(db, on_complete, ("workflowId", serde_json::to_value(id)?), result, now, origin)
            .await?;
    }
    db.replace(id, value).await
}

/// Cancel a running workflow (`reason` goes in its `error`). False if it had already finished.
pub(crate) async fn cancel(db: &MutationDb, id: Id<Workflow>, now: f64, reason: &str) -> crate::Result<bool> {
    let Some(wf) = db.get(id).await? else {
        return Err(UserError::new("not_found", format!("Workflow {id} does not exist")).into());
    };
    if wf.state.is_finished() {
        return Ok(false);
    }
    finish(db, now, wf, Ending::Canceled(reason.to_string())).await?;
    Ok(true)
}

/// Deliver event `name` to a running workflow: to its oldest step waiting for it (then the workflow advances), or
/// kept for the next `wait_for_event(name)`.
pub(crate) async fn send_event(
    db: &MutationDb,
    scheduler: &Scheduler,
    id: Id<Workflow>,
    name: &str,
    value: Value,
    now: f64,
) -> crate::Result<()> {
    let Some(wf) = db.get(id).await? else {
        return Err(UserError::new("not_found", format!("Workflow {id} does not exist")).into());
    };
    if wf.state.is_finished() {
        return Err(UserError::new("finished", format!("Workflow {id} has finished: it takes no more events")).into());
    }
    let waiting = steps_of(db, id)
        .await?
        .into_iter()
        .find(|s| s.kind == StepKind::Event && s.name == name && s.state == StepState::Running);
    match waiting {
        Some(step) => {
            let mut s = step.value;
            s.state = StepState::Success;
            s.result = Some(value);
            s.completed_at = Some(now);
            db.replace(step.id, s).await?;
            schedule_advance(scheduler, id).await
        }
        None => {
            let mut wf = wf;
            wf.value.events.push(super::tables::PendingEvent { name: name.to_string(), value });
            db.replace(id, wf.value).await
        }
    }
}
