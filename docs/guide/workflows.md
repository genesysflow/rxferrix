# Workflows

A `#[workflow]` is a durable function made of steps. Each step's outcome is journaled, so a workflow survives
restarts, can sleep for days and wait for events, and each mutation step runs exactly once:

```rust
#[workflow]
pub async fn onboard(step: &WorkflowCtx, user: Id<User>) -> Result<String> {
    let email = step.run_query::<email_of>((user,)).await?;
    step.run_action::<send_welcome>((email,)).with_retries(3).pool("emails").await?;
    step.sleep(Duration::from_secs(3 * 86_400)).await?;
    let answer: String = step.wait_for_event("survey").await?;   // ctx.workflows.send_event(id, "survey", ..)
    step.run_mutation::<save_answer>((user, answer.clone())).await?;
    Ok(answer)
}

// In a mutation (transactional) or an action:
let id: Id<Workflow> = ctx.workflows.on_complete::<onboarded>(user).start::<onboard>((user,)).await?;
```

The body runs again from the start whenever a step finishes. Steps already in the journal return their recorded
result at once, so the body must be deterministic: it touches the database and the outside world only through steps.
A failed step is an `Err` the body can handle. Steps run in parallel with `try_join!` (pass each as
`step.run_action::<f>(..).into_future()`), and `ctx.workflows.cancel(id)` stops a workflow. Its status is its
`_workflows` document, so a query can show progress live. The dashboard has a Workflows page. Details: [docs/CONTRACT.md](../CONTRACT.md) §1.7.1.
