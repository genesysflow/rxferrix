# Workflows

Use a workflow when a background operation has several durable steps, needs to sleep, or waits for an event. Use a [scheduled job](/guide/scheduler) for one delayed function call.

A workflow journals each step's result. After a restart, it replays the body and reuses completed results instead of repeating those steps.

## Define a workflow

This example assumes internal functions `email_of`, `send_welcome`, and `save_answer`, and a registered `emails` workpool:

```rust
use std::time::Duration;
use rxferrix::auth::User;
use rxferrix::prelude::*;

#[workflow]
pub async fn onboard(step: &WorkflowCtx, user: Id<User>) -> Result<String> {
    let email = step.run_query::<email_of>((user,)).await?;
    step.run_action::<send_welcome>((email,))
        .with_retries(3)
        .pool("emails")
        .await?;
    step.sleep(Duration::from_secs(3 * 86_400)).await?;
    let answer: String = step.wait_for_event("survey").await?;
    step.run_mutation::<save_answer>((user, answer.clone())).await?;
    Ok(answer)
}
```

The function references use the same typed arguments as actions and tests. A failed step returns an error the body can handle or propagate.

## Start it from app code

From an authorized mutation or action:

```rust
use rxferrix::workflow::Workflow;

let id: Id<Workflow> = ctx.workflows.start::<onboard>((user,)).await?;
```

Starting in a mutation is transactional. Store the ID if the app needs to send events, show progress, or cancel the operation. A workflow is started through `ctx.workflows`; it is not called like a client query.

Deliver an event with `ctx.workflows.send_event(id, "survey", answer).await?`. Protect any public mutation that sends events or cancels workflows with the app's access checks. Jobs and workflow steps do not inherit the scheduling user's identity; pass the data they need explicitly.

## Keep the body safe to replay

The body runs again from the beginning as progress is made. Database reads, writes, and external side effects belong inside `run_query`, `run_mutation`, or `run_action` steps. Do not perform them directly in the workflow body, and do not use random values or the system clock to choose a different step sequence on replay.

A mutation step commits its writes with its completion record. An action step can have external side effects before failing; retries may repeat those effects. Use idempotency for retryable calls.

Keep in-flight workflows in mind when changing their step order or meaning. Existing journals describe the previous sequence; a deploy should not reinterpret them as unrelated work.

## Track completion and cancellation

`ctx.workflows.on_complete::<callback>(context).start::<onboard>(args)` schedules a completion mutation. `ctx.workflows.cancel(id)` stops the workflow. `_workflows` stores status, and `_workflow_steps` stores step progress, so you can expose a live progress query. The dashboard's **Workflows** page displays both.

Steps can run concurrently with `try_join!` by passing each step's `.into_future()`. See the [workflow reference](/CONTRACT#_1-7-1-workflows) for callback signatures, event behavior, and replay details.
