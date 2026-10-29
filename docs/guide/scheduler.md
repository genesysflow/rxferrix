# Scheduled jobs and crons

Use the scheduler when work should run later, after a successful write, or on a recurring schedule. Jobs are persisted in the database and can be inspected in the dashboard.

Queries cannot be scheduled. Mutations and actions can schedule mutations or actions, including internal functions.

## Schedule work after a write

In a module containing the `add` mutation from [Functions and data](/guide/functions):

```rust
use std::time::Duration;
use rxferrix::prelude::*;
use rxferrix::scheduler::ScheduledJob;

#[mutation]
pub async fn add_later(ctx: &MutationCtx, text: String, delay_seconds: u32)
    -> Result<Id<ScheduledJob>>
{
    ensure_user!(!text.trim().is_empty(), "empty_text", "A task needs some text");
    ctx.scheduler.run_after::<add>(
        Duration::from_secs(delay_seconds.into()),
        (text,),
    ).await
}
```

Scheduling inside a mutation commits with that mutation: a rollback creates no job. Scheduling inside an action takes effect immediately in a separate transaction.

Use `run_at::<f>(at_ms, args)` for an absolute time in milliseconds since the Unix epoch. A time in the past means as soon as the scheduler can run it. Save the returned job ID if you need to cancel it.

## Pass the data the job needs

Scheduled functions run with **no user identity**, even when a signed-in user scheduled them. An auth check such as `require_user()` therefore fails inside the job.

Authorize the request in the public function, then pass the user ID and other needed data to an internal job function. Decide whether delayed work should recheck current permissions when it runs. Do not expose a public function that lets callers impersonate a user by supplying an arbitrary ID.

## Add recurring work

Register recurring jobs in the `RxFerrix` builder. For an internal `maintenance::purge` mutation:

```rust
rxferrix::RxFerrix::new()
    .cron::<maintenance::purge>("purge", "0 0 * * * *", ())
    .run()
```

Cron expressions have six fields: **second, minute, hour, day, month, weekday**. This example runs hourly in UTC.

Use `.cron_tz::<f>(name, expression, "Europe/Bucharest", args)` for local time, or `.interval::<f>(name, duration, args)` for a fixed interval. A spring-forward time that does not exist runs later by the gap; a repeated fall-back time runs once.

Missed runs are coalesced into one run on startup. A cron skips a run if its previous job is still running. Administrators can use **Scheduler → Run now**.

## Understand execution and retries

| Job | Behavior |
| --- | --- |
| Scheduled mutation | Its writes and completion record commit together; a restart cannot commit the writes without recording completion. |
| Scheduled action, no retries | One attempt; an interruption may have external side effects and is marked failed on recovery. |
| Scheduled action with retries | Failures and restart interruptions can retry while attempts remain. External side effects may repeat. |

Opt into action retries with `ctx.scheduler.with_retries(3).run_after::<f>(delay, args)`. Retries use exponential backoff. Retry options do not make failed mutations retry.

Give retryable external calls an idempotency key or another duplicate-handling strategy. A failed or canceled action cannot undo mutations it already committed.

## Limit concurrency with a workpool

Register a pool in `main.rs`:

```rust
use rxferrix::scheduler::Workpool;

rxferrix::RxFerrix::new()
    .workpool(Workpool::new("emails").max_parallelism(5).retries(3))
    .run()
```

Enqueue an action with `ctx.scheduler.pool("emails").enqueue::<send_email>((to,)).await?`. Each pool has independent slots and retry defaults. `on_complete::<callback>(context)` schedules a mutation after success, final failure, or cancellation; the callback takes the work ID, supplied context, and `WorkResult<Output>`.

## Cancel and inspect jobs

`ctx.scheduler.cancel(id).await?` cancels a pending job and signals a running action. Actions can check `ctx.is_canceled()` or await `ctx.canceled()`. After the default five-second grace period, a still-running action is aborted at an await point. `cancel_pool(name)` cancels a pool's queued and running work.

The dashboard shows pending and active jobs in `_scheduled_jobs`. Finished jobs move to history in `aux.db` after one hour by default and are retained for seven days. Tune this through `SchedulerOptions`.

In tests, use `TestApp::run_due_jobs()` and `advance_time(...)` instead of sleeping. For durable sequences with sleeps and events, see [Workflows](/guide/workflows). Full signatures are in the [scheduler reference](/CONTRACT#_1-7-scheduler-and-crons).
