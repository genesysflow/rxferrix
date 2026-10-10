//! `step: &WorkflowCtx`, the steps a workflow body awaits, and the journal they are replayed against.
//!
//! The body runs again from the start every time the workflow advances. Each step it starts gets the next index;
//! a step already in the journal resolves at once with its recorded outcome (`Poll::Ready`), and one that isn't is
//! recorded as new and never resolves in this run (`Poll::Pending`). When the body can't go further (every future
//! it awaits is pending), `super::engine` starts the new steps and the run ends; when one finishes, the body is run
//! again and gets further.

use std::collections::BTreeMap;
use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::tables::{StepKind, StepState, Workflow, WorkflowStep};
use crate::id::Id;
use crate::scheduler::Backoff;
use crate::schema::Doc;
use crate::{ActionFunction, MutationFunction, QueryFunction};

/// What a step calls, and how.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub kind: StepKind,
    /// The function's path, the event's name, or "sleep".
    pub name: String,
    pub args: Value,
    pub pool: Option<String>,
    pub retries: Option<u32>,
    pub backoff: Option<Backoff>,
}

/// A step the body started that isn't in the journal yet.
#[derive(Debug, Clone)]
pub(crate) struct NewStep {
    pub index: u32,
    pub call: Call,
}

/// The journal of one run of a body, and what the run found.
pub(crate) struct Replay {
    journal: BTreeMap<u32, Doc<WorkflowStep>>,
    next: u32,
    pub new_steps: Vec<NewStep>,
    /// The body doesn't match its journal: the workflow fails.
    pub fatal: Option<String>,
}

impl Replay {
    pub fn new(steps: Vec<Doc<WorkflowStep>>) -> Replay {
        Replay {
            journal: steps.into_iter().map(|s| (s.index, s)).collect(),
            next: 0,
            new_steps: Vec::new(),
            fatal: None,
        }
    }

    /// Steps of the journal still running.
    pub fn running(&self) -> impl Iterator<Item = &Doc<WorkflowStep>> {
        self.journal.values().filter(|s| s.state == StepState::Running)
    }

    /// The outcome of step `index`, if it has one; else it is recorded as new (or found running) and `None`.
    fn resolve(&mut self, index: u32, call: &Call) -> Option<Result<Value, String>> {
        let Some(step) = self.journal.get(&index) else {
            if !self.new_steps.iter().any(|s| s.index == index) {
                self.new_steps.push(NewStep { index, call: call.clone() });
            }
            return None;
        };
        // Args aren't compared: they went through JSON (floats may not round-trip exactly), and a body that computes
        // different args from the same history is broken in a way this can't catch anyway.
        if step.kind != call.kind || step.name != call.name {
            self.fatal.get_or_insert_with(|| {
                format!(
                    "the workflow is not deterministic: its step {index} was {} `{}` and is now {} `{}` (did its code \
                     change while it ran?)",
                    step.kind.as_str(),
                    step.name,
                    call.kind.as_str(),
                    call.name
                )
            });
            return None;
        }
        match step.state {
            StepState::Running => None,
            StepState::Success => Some(Ok(step.result.clone().unwrap_or(Value::Null))),
            StepState::Failed => Some(Err(step.error.clone().unwrap_or_else(|| "failed".to_string()))),
            StepState::Canceled => Some(Err(step.error.clone().unwrap_or_else(|| "canceled".to_string()))),
        }
    }
}

fn lock(replay: &Mutex<Replay>) -> std::sync::MutexGuard<'_, Replay> {
    replay.lock().unwrap_or_else(|e| e.into_inner())
}

/// The context of a `#[workflow]` body: it starts steps, and nothing else. A body must be **deterministic**: run
/// again with the same step outcomes, it must start the same steps in the same order. So it reads the database only
/// through steps (`run_query`), changes it through `run_mutation`, talks to the world through `run_action`, waits with
/// `sleep` (not `tokio::time::sleep`), and doesn't use the clock, randomness or anything else that changes between
/// runs (compute those in a step). It may run steps in parallel (`futures::try_join!`), but shouldn't race them
/// (`select!`): which one won could differ when it is replayed.
///
/// Each step is a future to `.await`, with options for mutations and actions:
/// `step.run_action::<send>((to,)).with_retries(3).pool("emails").await?`.
pub struct WorkflowCtx {
    replay: Arc<Mutex<Replay>>,
    id: Id<Workflow>,
}

impl WorkflowCtx {
    pub(crate) fn new(replay: Arc<Mutex<Replay>>, id: Id<Workflow>) -> WorkflowCtx {
        WorkflowCtx { replay, id }
    }

    /// This workflow's id.
    pub fn workflow_id(&self) -> Id<Workflow> {
        self.id
    }

    fn step<T>(&self, kind: StepKind, name: String, args: Result<Value, serde_json::Error>) -> Step<T> {
        let call = args.map(|args| Call { kind, name, args, pool: None, retries: None, backoff: None });
        Step { replay: self.replay.clone(), call: call.map_err(|e| e.to_string()), _output: PhantomData }
    }

    /// Run a query (public or internal), in the transaction that advances the workflow: it resolves right away.
    pub fn run_query<F: QueryFunction>(&self, args: impl Into<F::Args>) -> Step<F::Output> {
        self.step(StepKind::Query, F::path(), serde_json::to_value(args.into()))
    }

    /// Run a mutation (public or internal) as a scheduled job: exactly once.
    pub fn run_mutation<F: MutationFunction>(&self, args: impl Into<F::Args>) -> Step<F::Output> {
        self.step(StepKind::Mutation, F::path(), serde_json::to_value(args.into()))
    }

    /// Run an action (public or internal) as a scheduled job: at most once per attempt, retried on errors with
    /// [`Step::with_retries`]. An action interrupted by a restart fails ("server restarted while running").
    pub fn run_action<F: ActionFunction>(&self, args: impl Into<F::Args>) -> Step<F::Output> {
        self.step(StepKind::Action, F::path(), serde_json::to_value(args.into()))
    }

    /// Wait `duration` (durably: a restart doesn't cut it short or start it over).
    pub fn sleep(&self, duration: Duration) -> Step<()> {
        let ms = duration.as_secs_f64() * 1000.0;
        self.step(StepKind::Sleep, "sleep".to_string(), Ok(serde_json::json!({ "ms": ms })))
    }

    /// Wait for the event `name`, sent with `ctx.workflows.send_event(id, name, value)`, and get its value. An event
    /// sent before the workflow waits for it is kept until it does; each event is taken by one wait.
    pub fn wait_for_event<T: DeserializeOwned>(&self, name: &str) -> Step<T> {
        self.step(StepKind::Event, name.to_string(), Ok(Value::Null))
    }
}

/// A step, not started yet: `.await` it (or pass it to `try_join!`). Options apply to mutations and actions.
#[must_use = "a step does nothing unless awaited"]
pub struct Step<T> {
    replay: Arc<Mutex<Replay>>,
    call: Result<Call, String>,
    _output: PhantomData<fn() -> T>,
}

impl<T> Step<T> {
    /// Retry a failing action up to `retries` more times (with backoff), before the step fails. Overrides its pool's
    /// default. Ignored for other steps.
    pub fn with_retries(mut self, retries: u32) -> Self {
        if let Ok(call) = &mut self.call {
            call.retries = Some(retries);
        }
        self
    }

    /// Run the action in workpool `name` (`Ferrix::workpool`), with its parallelism and default retries.
    pub fn pool(mut self, name: &str) -> Self {
        if let Ok(call) = &mut self.call {
            call.pool = Some(name.to_string());
        }
        self
    }

    /// The action's retry backoff: `initial`, then `base` times longer each time (at most an hour).
    pub fn backoff(mut self, initial: Duration, base: f64) -> Self {
        if let Ok(call) = &mut self.call {
            call.backoff = Some(Backoff { initial_ms: initial.as_secs_f64() * 1000.0, base });
        }
        self
    }
}

impl<T: DeserializeOwned> IntoFuture for Step<T> {
    type Output = crate::Result<T>;
    type IntoFuture = StepFuture<T>;

    /// Takes the step's index: steps are numbered in the order they are started.
    fn into_future(self) -> StepFuture<T> {
        let index = {
            let mut replay = lock(&self.replay);
            replay.next += 1;
            replay.next - 1
        };
        StepFuture { replay: self.replay, index, call: self.call, _output: PhantomData }
    }
}

/// A started step: ready once it has an outcome in the journal.
pub struct StepFuture<T> {
    replay: Arc<Mutex<Replay>>,
    index: u32,
    call: Result<Call, String>,
    _output: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Future for StepFuture<T> {
    type Output = crate::Result<T>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let call = match &this.call {
            Ok(call) => call,
            Err(e) => return Poll::Ready(Err(anyhow::anyhow!("the step's args don't serialize: {e}"))),
        };
        // Pending without a wake-up: the engine runs the body again when the step finishes.
        let Some(outcome) = lock(&this.replay).resolve(this.index, call) else { return Poll::Pending };
        Poll::Ready(match outcome {
            Ok(value) => serde_json::from_value(value).map_err(|e| {
                anyhow::anyhow!("the result of step {} (`{}`) doesn't deserialize: {e}", this.index, call.name)
            }),
            Err(error) => Err(anyhow::anyhow!("{error}")),
        })
    }
}
