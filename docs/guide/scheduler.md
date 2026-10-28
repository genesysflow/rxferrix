# Scheduler and crons

Mutations and actions can schedule other mutations and actions (internal ones too), and `main` can declare crons:

```rust
use std::time::Duration;
use rxferrix::scheduler::ScheduledJob;

#[mutation]
pub async fn add_later(ctx: &MutationCtx, text: String, delay_seconds: u32) -> Result<Id<ScheduledJob>> {
    // Written in this mutation's transaction: scheduled only if it commits. Scheduling a query doesn't compile.
    ctx.scheduler.run_after::<add>(Duration::from_secs(delay_seconds.into()), (text,)).await
}

fn main() -> rxferrix::Result<()> {
    rxferrix::RxFerrix::new()
        .cron::<purge_completed>("purge-completed", "0 0 * * * *", ())   // sec min hour day month weekday, UTC
        .cron_tz::<daily_digest>("digest", "0 0 9 * * *", "Europe/Bucharest", ())   // 09:00 local, DST-aware
        .run()
}
```

Also `run_at::<f>(ms, args)`, `cancel(id)`, `with_retries(n)` (actions), and `.interval::<f>(name, Duration, args)`.
Canceling works on running actions too: the action sees `ctx.is_canceled()` (or awaits `ctx.canceled()` in a
`select!`) and should stop; after a 5 second grace period it is aborted at its next `.await`. Mutations it already
ran stay committed. Admins can run a cron immediately with `_system/scheduler:run_cron_now`.
Jobs live in the `_scheduled_jobs` table (so the dashboard and subscriptions see them live). Scheduled mutations run
**exactly once** (the job's completion commits with the mutation's writes); actions run **at most once** per attempt (an
action interrupted by a restart is marked failed, or retried if it was scheduled `with_retries`; a server that stops
waits a few seconds for running actions first). Scheduled functions run with no user identity; the execution log shows
them as `scheduler`. Missed cron runs run once at startup, a cron whose previous run is still going is skipped, and
finished jobs move to the job history in `aux.db` an hour after they finish (`SchedulerOptions::history_after`), so
`data.db` and its backups don't grow with every job run; they are deleted after 7 days (`retention`). In a time zone, a run in the hour skipped by a spring-forward change runs
later by the length of the gap (usually an hour), and a time of day in the repeated fall-back hour runs once. In tests, `TestApp::advance_time(d)` / `run_due_jobs()` run due jobs
deterministically. Details: [docs/CONTRACT.md](../CONTRACT.md) §1.7.

**Workpools** limit how many actions of one kind run at once, with default retries, and **`on_complete`** runs a
mutation with the outcome once a job has finished (exactly once: the transaction that records the outcome schedules
it, and it runs in its own transaction):

```rust
rxferrix::RxFerrix::new().workpool(Workpool::new("emails").max_parallelism(5).retries(3))

ctx.scheduler.pool("emails").on_complete::<sent>(user).enqueue::<send_email>((to,)).await?;
// sent(ctx, work_id: Id<ScheduledJob>, context: Id<User>, result: WorkResult<String>)
```

Each pool has its own slots, so a full pool doesn't hold up the others. `ctx.scheduler.cancel_pool("emails")`
cancels everything queued or running in it.
