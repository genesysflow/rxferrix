<script lang="ts">
  import { untrack } from "svelte";
  import { RxFerrixError } from "@rxferrix/client";
  import { useMutation, usePaginatedQuery, useQuery } from "@rxferrix/svelte";
  import { fetchJobHistory, retryFromHistory } from "../lib/admin.js";
  import { scheduler, type CronSummary, type JobHistoryFilter, type JobState, type ScheduledJob } from "../lib/api.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatRelative, formatTime } from "../lib/format.js";
  import { href, router, type Route } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import JobDetail from "../components/JobDetail.svelte";

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

  // The job history (aux.db): finished jobs moved out of `_scheduled_jobs`. Read over HTTP, not live.
  type HistoryState = NonNullable<JobHistoryFilter["state"]>;
  const historyFilters: { state: HistoryState | null; label: string }[] = [
    { state: null, label: "All" },
    { state: "success", label: "Success" },
    { state: "failed", label: "Failed" },
    { state: "canceled", label: "Canceled" },
  ];
  const historyPage = 50;
  let historyState = $state<HistoryState | null>(null);
  let history = $state<ScheduledJob[]>([]);
  let historyMore = $state(false);
  let historyLoading = $state(false);
  let historyError = $state<string | null>(null);
  let historyOpen = $state<string | null>(null);
  // Only the latest request's answer is shown (the filter may change while one is in flight).
  let historyRequest = 0;

  async function loadHistory(reset: boolean) {
    const request = ++historyRequest;
    const last = reset ? undefined : history.at(-1);
    const cursor = last ? { beforeCompletedAt: last.completedAt ?? last.scheduledAt, beforeId: last._id } : {};
    historyLoading = true;
    historyError = null;
    try {
      const jobs = await fetchJobHistory(ctx.auth, { state: historyState, limit: historyPage, ...cursor });
      if (request !== historyRequest) return;
      history = reset ? jobs : [...history, ...jobs];
      historyMore = jobs.length === historyPage;
    } catch (e) {
      if (request === historyRequest) historyError = e instanceof RxFerrixError ? e.message : String(e);
    } finally {
      if (request === historyRequest) historyLoading = false;
    }
  }

  $effect(() => {
    void historyState;
    untrack(() => loadHistory(true));
  });

  async function runAgain(job: ScheduledJob) {
    try {
      await retryFromHistory(ctx.auth, job._id);
      ctx.toast("Scheduled a new run of the job");
    } catch (e) {
      ctx.toast(e instanceof RxFerrixError ? e.message : String(e), "error");
    }
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
                  <td colspan="7"><JobDetail {job} /></td>
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

    <section aria-labelledby="history-heading">
      <div class="jobs-head">
        <h2 id="history-heading">History</h2>
        <div class="filters" role="group" aria-label="Filter the history by state">
          {#each historyFilters as f (f.label)}
            <button class="small" class:active={historyState === f.state} aria-pressed={historyState === f.state} onclick={() => (historyState = f.state)}>
              {f.label}
            </button>
          {/each}
        </div>
        <button class="small refresh" onclick={() => loadHistory(true)} disabled={historyLoading}>Refresh</button>
      </div>
      <p class="muted pad small-text">
        Finished jobs move here from the list above once they are an hour old (<code class="mono">SchedulerOptions::history_after</code>), into
        <code class="mono">aux.db</code>, so <code class="mono">data.db</code> and its backups don't grow with every job run. Kept 7 days by default. Not live: refresh to see newer ones.
      </p>
      {#if historyError}
        <div class="alert" role="alert">{historyError}</div>
      {:else if historyLoading && history.length === 0}
        <Empty title="Loading…" />
      {:else if history.length === 0}
        <Empty title={historyState ? `No ${stateLabels[historyState]} jobs in the history` : "The history is empty"} />
      {:else}
        <table class="grid" data-testid="job-history">
          <thead>
            <tr><th>Function</th><th>State</th><th>Finished</th><th>Attempts</th><th>Origin</th><th></th></tr>
          </thead>
          <tbody>
            {#each history as job (job._id)}
              <tr onclick={() => (historyOpen = historyOpen === job._id ? null : job._id)} class:selected={historyOpen === job._id} aria-expanded={historyOpen === job._id}>
                <td>
                  <span class="badge {job.kind}">{job.kind}</span> <span class="mono">{job.path}</span>
                  {#if job.pool}<span class="badge pool" title="Workpool">{job.pool}</span>{/if}
                </td>
                <td><span class="badge state {job.state}">{stateLabels[job.state]}</span></td>
                <td title={job.completedAt ? formatTime(job.completedAt) : ""}>{job.completedAt ? formatRelative(job.completedAt, now) : ""}</td>
                <td>{job.attempts}{job.maxAttempts > 1 ? ` / ${job.maxAttempts}` : ""}</td>
                <td class="mono faint">{job.origin ?? ""}</td>
                <td class="actions" onclick={(e) => e.stopPropagation()}>
                  <button class="small" onclick={() => runAgain(job)}>Retry</button>
                </td>
              </tr>
              {#if historyOpen === job._id}
                <tr class="detail">
                  <td colspan="6"><JobDetail {job} inHistory /></td>
                </tr>
              {/if}
            {/each}
          </tbody>
        </table>
        <div class="more">
          {#if historyMore}<button onclick={() => loadHistory(false)} disabled={historyLoading}>Load more</button>{/if}
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
  .refresh {
    margin-top: 6px;
  }
  .more {
    padding: 12px 20px;
  }
</style>
