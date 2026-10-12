//! Durable workflows: long-running functions whose progress survives restarts, made of steps (queries, mutations,
//! actions, sleeps, events).
//!
//! ```ignore
//! #[workflow]
//! pub async fn onboard(step: &WorkflowCtx, user: Id<User>) -> Result<String> {
//!     let email = step.run_query::<email_of>((user,)).await?;
//!     step.run_action::<send_welcome>((email.clone(),)).with_retries(3).pool("emails").await?;
//!     step.sleep(Duration::from_secs(3 * 86_400)).await?;
//!     let answer: String = step.wait_for_event("survey").await?;
//!     step.run_mutation::<save_answer>((user, answer.clone())).await?;
//!     Ok(answer)
//! }
//!
//! #[mutation]
//! pub async fn sign_up(ctx: &MutationCtx, ...) -> Result<Id<Workflow>> {
//!     let user = ctx.db.insert(...).await?;
//!     ctx.workflows.on_complete::<onboarded>(user).start::<onboard>((user,)).await   // starts only if this commits
//! }
//! ```
//!
//! How it works ([`engine`]): a workflow's steps are journaled in `_workflow_steps`. Its body runs inside a
//! mutation, and is run again from the start whenever a step finishes: steps already in the journal return their
//! recorded outcome at once, so it gets as far as it can, then waits for the steps it just started. The body must
//! therefore be deterministic ([`WorkflowCtx`]). Mutation and action steps run as scheduled jobs (in workpools, with
//! retries: `crate::scheduler`) whose completion advances the workflow in the same transaction that records it, so
//! each step's outcome is recorded once; query steps run inside the advancing transaction.
//!
//! - [`Workflows`] (`ctx.workflows` in mutations and actions): `start`, `on_complete(..).start`, `cancel`,
//!   `send_event`. In a mutation they are part of its transaction.
//! - [`tables`]: `_workflows` ([`Workflow`]) and `_workflow_steps` ([`WorkflowStep`]). A workflow's status is its
//!   document: `ctx.db.get(id)` in a query (live).
//! - Finished workflows (and their steps) are deleted with the scheduler's retention (`SchedulerOptions::retention`,
//!   7 days).
//! - System functions `_system/workflow:*`: `advance`, `step_done`, `wake` (the engine's jobs), `start`, `cancel`,
//!   `send_event` (behind `ctx.workflows` in actions), and the dashboard's `list`, `steps`, `cancel_workflow`.

pub mod ctx;
pub mod engine;
pub mod tables;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use ctx::{Step, StepFuture, WorkflowCtx};
pub use tables::{PendingEvent, StepKind, StepState, Workflow, WorkflowState, WorkflowStep};

use crate::db::WriteTxn;
use crate::db::query::MutationDb;
use crate::prelude::*;
use crate::private::{WORKFLOWS, WorkflowDef};
use crate::runtime::{App, Identity};
use crate::scheduler::store::{self, WorkResult};
use crate::scheduler::{JobKind, OnComplete, ScheduledJob, Scheduler, Workpool};
use crate::{FunctionRef, MutationFunction};

/// A `#[workflow]`: what `ctx.workflows.start::<W>(args)` takes. Implemented by the macro.
pub trait WorkflowFunction: FunctionRef {}

/// Every `#[workflow]` linked into the binary, by path.
pub fn registry() -> crate::Result<&'static BTreeMap<String, &'static WorkflowDef>> {
    static REGISTRY: OnceLock<Result<BTreeMap<String, &'static WorkflowDef>, String>> = OnceLock::new();
    let built = REGISTRY.get_or_init(|| {
        let mut by_path = BTreeMap::new();
        for def in WORKFLOWS.iter() {
            if by_path.insert(def.path(), def).is_some() {
                return Err(format!("two workflows are registered at path `{}`", def.path()));
            }
        }
        Ok(by_path)
    });
    built.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Workflow args may be at most this large (serialized).
pub const MAX_ARGS_BYTES: usize = store::MAX_ARGS_BYTES;

/// Starts, cancels and signals workflows: `ctx.workflows` in mutations and actions. In a mutation everything is
/// part of its transaction (a workflow started by a mutation that rolls back never existed); in an action each call
/// takes effect at once (through the internal system mutations `_system/workflow:start`, `:cancel`, `:send_event`).
pub struct Workflows {
    target: Target,
}

enum Target {
    Mutation { txn: Arc<WriteTxn>, scheduler: Scheduler, origin: String },
    Action { app: App, identity: Identity, origin: String },
}

impl Workflows {
    pub(crate) fn in_mutation(txn: Arc<WriteTxn>, origin: &str, pools: Arc<BTreeMap<String, Workpool>>) -> Workflows {
        let scheduler = Scheduler::in_mutation(txn.clone(), origin, pools);
        Workflows { target: Target::Mutation { txn, scheduler, origin: origin.to_string() } }
    }

    pub(crate) fn in_action(app: App, identity: Identity, origin: &str) -> Workflows {
        Workflows { target: Target::Action { app, identity, origin: origin.to_string() } }
    }

    /// Start workflow `W` with `args`. Returns its id: its status is `ctx.db.get(id)` (a [`Workflow`]). Its first
    /// steps start right after this commits (in a scheduled job).
    pub async fn start<W: WorkflowFunction>(&self, args: impl Into<W::Args>) -> crate::Result<Id<Workflow>> {
        self.start_path(W::path(), serde_json::to_value(args.into())?, None).await
    }

    /// Once the workflow has finished, run mutation `F` with `{ workflowId, context, result }`
    /// ([`WorkResult`]: its return value, why it failed, or `canceled`):
    /// `ctx.workflows.on_complete::<done>(context).start::<W>(args)`.
    pub fn on_complete<F: MutationFunction>(&self, context: impl Serialize) -> StartWith<'_> {
        let context = serde_json::to_value(context).map_err(|e| e.to_string());
        StartWith { workflows: self, on_complete: context.map(|context| OnComplete { path: F::path(), context }) }
    }

    /// Cancel a running workflow: it stops where it is (its running steps are canceled; mutations and actions that
    /// already ran stay done), and its `on_complete` runs with `canceled`. Canceling a finished workflow does
    /// nothing. `not_found` (a user error) for an unknown id.
    pub async fn cancel(&self, id: Id<Workflow>) -> crate::Result<()> {
        match &self.target {
            Target::Mutation { txn, origin, .. } => {
                engine::cancel(&MutationDb::new(txn.clone()), id, txn.now(), &format!("Canceled by {origin}")).await?;
            }
            Target::Action { app, identity, origin } => {
                app.typed_mutation::<cancel>((id, format!("Canceled by {origin}")).into(), identity).await?;
            }
        }
        Ok(())
    }

    /// Send event `name` with `value` to a running workflow: its oldest `step.wait_for_event(name)` gets it, or the
    /// next one if none is waiting. `not_found` for an unknown id, `finished` for a workflow that has finished.
    pub async fn send_event(&self, id: Id<Workflow>, name: &str, value: impl Serialize) -> crate::Result<()> {
        let value = serde_json::to_value(value)?;
        match &self.target {
            Target::Mutation { txn, scheduler, .. } => {
                engine::send_event(&MutationDb::new(txn.clone()), scheduler, id, name, value, txn.now()).await
            }
            Target::Action { app, identity, .. } => {
                app.typed_mutation::<send_event>((id, name.to_string(), value).into(), identity).await?;
                Ok(())
            }
        }
    }

    async fn start_path(
        &self,
        name: String,
        args: Value,
        on_complete: Option<OnComplete>,
    ) -> crate::Result<Id<Workflow>> {
        match &self.target {
            Target::Mutation { txn, scheduler, origin } => {
                start_in(
                    &MutationDb::new(txn.clone()),
                    scheduler,
                    name,
                    args,
                    on_complete,
                    Some(origin.clone()),
                    txn.now(),
                )
                .await
            }
            Target::Action { app, identity, origin } => {
                let args = (name, args, on_complete, Some(origin.clone()));
                Ok(app.typed_mutation::<start>(args.into(), identity).await?.0)
            }
        }
    }
}

/// [`Workflows::start`] with an `on_complete`, from [`Workflows::on_complete`].
pub struct StartWith<'a> {
    workflows: &'a Workflows,
    on_complete: Result<OnComplete, String>,
}

impl StartWith<'_> {
    pub async fn start<W: WorkflowFunction>(self, args: impl Into<W::Args>) -> crate::Result<Id<Workflow>> {
        let on_complete = self.on_complete.map_err(|e| anyhow::anyhow!("on_complete context: {e}"))?;
        self.workflows.start_path(W::path(), serde_json::to_value(args.into())?, Some(on_complete)).await
    }
}

/// Insert a running workflow and schedule its first advance, in the caller's transaction.
async fn start_in(
    db: &MutationDb,
    scheduler: &Scheduler,
    name: String,
    args: Value,
    on_complete: Option<OnComplete>,
    origin: Option<String>,
    now: f64,
) -> crate::Result<Id<Workflow>> {
    if !registry()?.contains_key(&name) {
        return Err(UserError::new("not_found", format!("No workflow is registered at `{name}`")).into());
    }
    if let Some(on_complete) = &on_complete
        && store::kind_of(&on_complete.path)? != JobKind::Mutation
    {
        return Err(anyhow::anyhow!("on_complete must be a mutation, and `{}` is an action", on_complete.path));
    }
    let size = serde_json::to_string(&args)?.len();
    anyhow::ensure!(
        size <= MAX_ARGS_BYTES,
        "args for workflow `{name}` are too large ({size} bytes, max {MAX_ARGS_BYTES})"
    );
    let workflow = Workflow {
        name,
        args,
        state: WorkflowState::Running,
        result: None,
        error: None,
        steps: 0,
        started_at: now,
        completed_at: None,
        origin,
        on_complete,
        events: Vec::new(),
    };
    let id = db.insert(workflow).await?;
    engine::schedule_advance(scheduler, id).await?;
    Ok(id)
}

/// Delete workflows that finished before `cutoff` (ms since the epoch), with their steps, a batch at a time.
/// Returns how many workflows.
pub(crate) async fn cleanup(app: &App, cutoff: f64) -> crate::Result<usize> {
    const BATCH: usize = 64;
    let mut total = 0;
    loop {
        let deleted = store::write(app, |db| async move {
            let old = db
                .query::<Workflow>()
                .with_index(Workflow::by_completed_at, |q| {
                    q.gt(Workflow::completed_at, None).lt(Workflow::completed_at, Some(cutoff))
                })
                .take(BATCH)
                .await?;
            for wf in &old {
                let steps = db
                    .query::<WorkflowStep>()
                    .with_index(WorkflowStep::by_workflow, |q| q.eq(WorkflowStep::workflow, wf.id))
                    .collect()
                    .await?;
                for step in steps {
                    db.delete(step.id).await?;
                }
                db.delete(wf.id).await?;
            }
            Ok(old.len())
        })
        .await?;
        total += deleted;
        if deleted < BATCH {
            return Ok(total);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The engine's jobs
// ---------------------------------------------------------------------------------------------------------------------

/// Run the workflow's body and start the steps it waits for (see [`engine`]).
#[internal_mutation]
pub async fn advance(ctx: &MutationCtx, workflow: Id<Workflow>) -> Result<()> {
    engine::advance(ctx, workflow).await
}

/// The `on_complete` of every mutation and action step: record its outcome, and advance.
#[internal_mutation]
pub async fn step_done(
    ctx: &MutationCtx,
    work_id: Id<ScheduledJob>,
    context: Id<WorkflowStep>,
    result: WorkResult,
) -> Result<()> {
    let _ = work_id;
    engine::step_done(ctx, context, result).await
}

/// A sleep step has ended: advance.
#[internal_mutation]
pub async fn wake(ctx: &MutationCtx, step: Id<WorkflowStep>) -> Result<()> {
    engine::step_done(ctx, step, WorkResult::Success { return_value: Value::Null }).await
}

// ---------------------------------------------------------------------------------------------------------------------
// Behind `ctx.workflows` in actions
// ---------------------------------------------------------------------------------------------------------------------

/// Start the workflow at `name`. What `ctx.workflows.start` runs from an action.
#[internal_mutation]
pub async fn start(
    ctx: &MutationCtx,
    name: String,
    args: Value,
    on_complete: Option<OnComplete>,
    origin: Option<String>,
) -> Result<Id<Workflow>> {
    start_in(&ctx.db, &ctx.scheduler, name, args, on_complete, origin, ctx.now()).await
}

/// Cancel a workflow. What `ctx.workflows.cancel` runs from an action.
#[internal_mutation]
pub async fn cancel(ctx: &MutationCtx, id: Id<Workflow>, reason: String) -> Result<bool> {
    engine::cancel(&ctx.db, id, ctx.now(), &reason).await
}

/// Send an event to a workflow. What `ctx.workflows.send_event` runs from an action.
#[internal_mutation]
pub async fn send_event(ctx: &MutationCtx, id: Id<Workflow>, name: String, value: Value) -> Result<()> {
    engine::send_event(&ctx.db, &ctx.scheduler, id, &name, value, ctx.now()).await
}

// ---------------------------------------------------------------------------------------------------------------------
// Dashboard (admin only)
// ---------------------------------------------------------------------------------------------------------------------

/// Workflows, live and paginated, newest first; with `state`, only those in that state.
#[query]
pub async fn list(
    ctx: &QueryCtx,
    state: Option<WorkflowState>,
    pagination_opts: PaginationOpts,
) -> Result<PaginationResult<Doc<Workflow>>> {
    ctx.auth.require_admin()?;
    let query = match state {
        Some(state) => ctx.db.query::<Workflow>().with_index(Workflow::by_state, |q| q.eq(Workflow::state, state)),
        None => ctx.db.query::<Workflow>(),
    };
    query.order(Order::Desc).paginate(pagination_opts).await
}

/// A workflow's steps, in order. Live.
#[query]
pub async fn steps(ctx: &QueryCtx, id: String) -> Result<Vec<Doc<WorkflowStep>>> {
    ctx.auth.require_admin()?;
    let id: Id<Workflow> =
        id.parse().map_err(|_| UserError::new("invalid_id", format!("`{id}` is not a workflow id")))?;
    ctx.db
        .query::<WorkflowStep>()
        .with_index(WorkflowStep::by_workflow, |q| q.eq(WorkflowStep::workflow, id))
        .collect()
        .await
}

/// What [`cancel_workflow`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(crate = "::rxferrix::__private::ts_rs", rename_all = "camelCase")]
pub enum CancelWorkflowOutcome {
    Canceled,
    AlreadyFinished,
}

/// Cancel a running workflow from the dashboard.
#[mutation]
pub async fn cancel_workflow(ctx: &MutationCtx, id: String) -> Result<CancelWorkflowOutcome> {
    ctx.auth.require_admin()?;
    let id: Id<Workflow> =
        id.parse().map_err(|_| UserError::new("invalid_id", format!("`{id}` is not a workflow id")))?;
    let who = ctx.auth.identity().log_summary().unwrap_or_else(|| "an admin".to_string());
    let canceled = engine::cancel(&ctx.db, id, ctx.now(), &format!("Canceled from the dashboard by {who}")).await?;
    Ok(if canceled { CancelWorkflowOutcome::Canceled } else { CancelWorkflowOutcome::AlreadyFinished })
}
