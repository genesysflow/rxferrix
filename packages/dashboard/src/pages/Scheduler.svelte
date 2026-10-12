<script lang="ts">
  import { RxFerrixError } from "@rxferrix/client";
  import { useMutation, usePaginatedQuery, useQuery } from "@rxferrix/svelte";
  import { scheduler, type CronSummary, type JobState, type ScheduledJob } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatRelative, formatTime, pretty, prettyArgs } from "../lib/format.js";
  import { href, link, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import IdLink from "../components/IdLink.svelte";

  let { route }: { route: Route } = $props();
  const ctx = dashboardContext();

  const filters: { state: JobState | null; label: string }[] = [
    { state: null, label: "All" },
    { state: "pending", label: "Pending" },
    { state: "inProgress", label: "In progress" },
    { state: "success", label: "Success" },
    { state: "failed", label: "Failed" },
    { state: "canceled", label: "Canceled" },
  ];
  const stateLabels: Record<JobState, string> = {
    pending: "pending",
    inProgress: "in progress",
    success: "success",
    failed: "failed",
    canceled: "canceled",
  };

  const filterState = $derived.by((): JobState | null => {
    const s = route.params.get("state");
    return filters.some((f) => f.state === s) ? (s as JobState) : null;
  });
  const jobs = usePaginatedQuery(scheduler.jobs, () => ({ state: filterState }), { initialNumItems: 50 });
  const crons = useQuery(scheduler.crons, () => ({}));
  const cancelJob = useMutation(scheduler.cancelJob);
  const runJobNow = useMutation(scheduler.runJobNow);
  const runCronNow = useMutation(scheduler.runCronNow);
  const appCrons = $derived((crons.data ?? []).filter((c) => !c.system));
  const systemCrons = $derived((crons.data ?? []).filter((c) => c.system));
  let startingCron = $state<string | null>(null);
  let open = $state<string | null>(null);

  // Relative times ("in 5m") tick.
  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });

  function setFilter(s: JobState | null) {
    router.navigate(href("scheduler", [], { state: s }));
  }

  async function cancel(job: ScheduledJob) {
    if (job.state === "inProgress" && !confirm(`Stop the running action ${job.path}? It is signaled now and aborted at its next await after a grace period; mutations it already committed stay.`)) return;
    try {
      const outcome = await cancelJob({ id: job._id });
      if (outcome === "canceled") ctx.toast(job.state === "inProgress" ? "Canceling the running action" : "Job canceled");
      else ctx.toast("The job had already finished", "error");
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    }
  }

  /** "Run now" for a cron; when its previous run hasn't finished, offer to run it anyway (`force`). */
  async function runCron(c: CronSummary) {
    startingCron = c.name;
    try {
      try {
        await runCronNow({ name: c.name });
      } catch (e) {
        if (!(e instanceof RxFerrixError && e.code === "running")) throw e;
        if (!confirm(`The previous run of “${c.name}” hasn't finished yet. Start another run anyway?`)) return;
        await runCronNow({ name: c.name, force: true });
      }
      ctx.toast(`Started ${c.name}`);
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    } finally {
      startingCron = null;
    }
  }

  async function runNow(job: ScheduledJob) {
    try {
      const id = await runJobNow({ id: job._id });
      ctx.toast(id === job._id ? "The job will run now" : "Scheduled a new run of the job");
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    }
  }

  function prettyResult(result: string | null): string {
    return result === null ? "" : prettyArgs(result);
  }
</script>

{#snippet cronRow(c: CronSummary)}
  <tr class:system={c.system}>
    <td><strong class:mono={c.system}>{c.name}</strong>{#if c.system}<span class="badge sys">system</span>{/if}</td>
    <td class="mono">{c.schedule}</td>
    <td class:faint={!c.timezone}>{c.timezone ?? "UTC"}</td>
    <td class="mono">{c.path}</td>
    <td title={c.lastRun ? formatTime(c.lastRun) : ""}>{c.lastRun ? formatRelative(c.lastRun, now) : "never"}</td>
    <td title={c.nextRun ? formatTime(c.nextRun) : ""}>{c.nextRun ? formatRelative(c.nextRun, now) : "never"}</td>
    <td>
      {#if c.lastJobState}<span class="badge state {c.lastJobState}">{stateLabels[c.lastJobState]}</span>{/if}
    </td>
    <td class="actions">
      <button class="small" onclick={() => runCron(c)} disabled={startingCron === c.name} title="Enqueue a run now; the schedule is unchanged">
        {startingCron === c.name ? "Starting…" : "▶ Run now"}
      </button>
    </td>
  </tr>
{/snippet}

<div class="page">
  <header class="page-header">
    <h1>Scheduler</h1>
    <span class="spacer"></span>
    <span class="muted small-text">Times are local; cron expressions run in their time zone (UTC unless set).</span>
  </header>
  <div class="body">
    <section aria-labelledby="crons-heading">
      <h2 id="crons-heading">Crons</h2>
      {#if crons.error}
        <div class="alert" role="alert">{crons.error.message}</div>
      {:else if crons.data && crons.data.length === 0}
        <p class="muted pad">No crons are declared. Add one with <code class="mono">RxFerrix::new().cron::&lt;f&gt;("name", "0 */5 * * * *", args)</code>.</p>
      {:else if crons.data}
        <table class="grid" data-testid="crons">
          <thead>
            <tr><th>Name</th><th>Schedule</th><th>Time zone</th><th>Function</th><th>Last run</th><th>Next run</th><th>Last job</th><th></th></tr>
          </thead>
          <tbody>
            {#each appCrons as c (c._id)}
              {@render cronRow(c)}
            {/each}
            {#if systemCrons.length > 0}
              <tr class="group"><td colspan="8">System crons <span class="faint">· run by rxferrix itself</span></td></tr>
              {#each systemCrons as c (c._id)}
                {@render cronRow(c)}
              {/each}
            {/if}
          </tbody>
        </table>
      {/if}
    </section>

    <section aria-labelledby="jobs-heading">
      <div class="jobs-head">
        <h2 id="jobs-heading">Jobs</h2>
        <div class="filters" role="group" aria-label="Filter jobs by state">
          {#each filters as f (f.label)}
            <button class="small" class:active={filterState === f.state} aria-pressed={filterState === f.state} onclick={() => setFilter(f.state)}>
              {f.label}
            </button>
          {/each}
        </div>
      </div>
      {#if jobs.error}
        <div class="alert" role="alert">{jobs.error.message}</div>
      {:else if jobs.isLoading && jobs.results.length === 0}
        <Empty title="Loading…" />
      {:else if jobs.results.length === 0}
        <Empty
          title={filterState ? `No ${stateLabels[filterState]} jobs` : "No scheduled jobs"}
          hint="Jobs appear here when a function calls ctx.scheduler.run_after / run_at / pool(..).enqueue, a cron runs, or a workflow runs a step."
        />
      {:else}
        <table class="grid" data-testid="jobs">
          <thead>
            <tr><th>Function</th><th>State</th><th>Run at</th><th>Attempts</th><th>Origin</th><th>Finished</th><th></th></tr>
          </thead>
          <tbody>
            {#each jobs.results as job (job._id)}
              <tr onclick={() => (open = open === job._id ? null : job._id)} class:selected={open === job._id} aria-expanded={open === job._id}>
                <td>
                  <span class="badge {job.kind}">{job.kind}</span> <span class="mono">{job.path}</span>
                  {#if job.pool}<span class="badge pool" title="Workpool">{job.pool}</span>{/if}
                </td>
                <td><span class="badge state {job.state}">{stateLabels[job.state]}</span></td>
                <td title={formatTime(job.runAt)}>{job.state === "pending" ? formatRelative(job.runAt, now) : formatTime(job.runAt)}</td>
                <td>{job.attempts}{job.maxAttempts > 1 ? ` / ${job.maxAttempts}` : ""}</td>
                <td class="mono faint">{job.origin ?? ""}</td>
                <td class="faint">{job.completedAt ? formatTime(job.completedAt) : ""}</td>
                <td class="actions" onclick={(e) => e.stopPropagation()}>
                  {#if job.state === "pending"}
                    <button class="small" onclick={() => runNow(job)}>Run now</button>
                    <button class="small danger" onclick={() => cancel(job)}>Cancel</button>
                  {:else if job.state === "inProgress"}
                    <button class="small danger" onclick={() => cancel(job)} title="Signal the running action to stop">Cancel</button>
                  {:else}
                    <button class="small" onclick={() => runNow(job)}>Retry</button>
                  {/if}
                </td>
              </tr>
              {#if open === job._id}
                <tr class="detail">
                  <td colspan="7">
                    <div class="detail-grid">
                      <div>
                        <h3>Args</h3>
                        <pre>{pretty(job.args)}</pre>
                      </div>
                      {#if job.error}
                        <div>
                          <h3>Error</h3>
                          <pre class="error-text">{job.error}</pre>
                        </div>
                      {/if}
                      {#if job.result !== null}
                        <div>
                          <h3>Result</h3>
                          <pre>{prettyResult(job.result)}</pre>
                        </div>
                      {/if}
                      <div class="times">
                        <h3>Timeline</h3>
                        <dl>
                          <dt>Scheduled</dt><dd>{formatTime(job.scheduledAt)}</dd>
                          <dt>Due</dt><dd>{formatTime(job.runAt)}</dd>
                          <dt>Started</dt><dd>{job.startedAt ? formatTime(job.startedAt) : "—"}</dd>
                          <dt>Finished</dt><dd>{job.completedAt ? formatTime(job.completedAt) : "—"}</dd>
                          {#if job.pool}<dt>Workpool</dt><dd class="mono">{job.pool}</dd>{/if}
                          {#if job.onComplete}<dt>On complete</dt><dd class="mono">{job.onComplete.path}</dd>{/if}
                          {#if job.origin?.startsWith("workflow:")}
                            <dt>Workflow</dt>
                            <dd><a href={href("workflows", [], { id: job.origin.slice("workflow:".length) })} onclick={link}>{job.origin.slice("workflow:".length)}</a></dd>
                          {/if}
                          <dt>Document</dt><dd><IdLink id={job._id} /></dd>
                        </dl>
                        <a href={href("logs")} onclick={link}>Execution logs</a>
                      </div>
                    </div>
                  </td>
                </tr>
              {/if}
            {/each}
          </tbody>
        </table>
        <div class="more">
          {#if jobs.status === "CanLoadMore"}<button onclick={() => jobs.loadMore(50)}>Load more</button>{/if}
        </div>
      {/if}
    </section>
  </div>
</div>

<style>
  .body {
    flex: 1;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 18px;
    padding-bottom: 24px;
  }
  h2 {
    margin: 14px 20px 8px;
  }
  .pad {
    padding: 0 20px;
  }
  .small-text {
    font-size: 12.5px;
  }
  .jobs-head {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .filters {
    display: flex;
    gap: 4px;
    margin-top: 6px;
  }
  .filters button.active {
    background: var(--accent-soft);
    color: var(--accent-text);
    border-color: transparent;
    font-weight: 600;
  }
  tr.group td {
    background: var(--panel-2);
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    cursor: default;
  }
  tr.system td {
    color: var(--muted);
  }
  .badge.sys,
  .badge.pool {
    margin-left: 6px;
  }
  table.grid tbody tr.group:hover,
  table.grid tbody tr.system:hover {
    background: none;
  }
  .badge.state.pending {
    color: var(--link);
  }
  .badge.state.inProgress {
    color: var(--warn);
    background: var(--warn-soft);
    border-color: transparent;
  }
  .badge.state.failed {
    color: var(--err);
    background: var(--err-soft);
    border-color: transparent;
  }
  .badge.state.canceled {
    color: var(--muted);
  }
  tr.detail {
    cursor: default;
    background: var(--panel-2);
  }
  tr.detail td {
    max-width: none;
    padding: 8px 16px 14px;
  }
  .detail-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 16px;
  }
  .detail-grid pre {
    max-height: 240px;
    overflow: auto;
    white-space: pre-wrap;
    word-break: break-word;
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 2px 12px;
    margin: 0 0 8px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
  }
  .more {
    padding: 12px 20px;
  }
</style>
