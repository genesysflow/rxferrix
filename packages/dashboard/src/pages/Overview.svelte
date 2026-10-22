<script lang="ts">
  import { onMount, type Snippet } from "svelte";
  import type { FunctionStats, Stats, Timeseries } from "../lib/api.js";
  import { fetchStats, fetchTimeseries } from "../lib/admin.js";
  import { fillBuckets, RANGES, rangeWindow, totals } from "../lib/chart.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatBytes, formatDuration, formatSpan, formatTime, formatUptime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";
  import LineChart from "../components/LineChart.svelte";
  import Panel from "../components/Panel.svelte";

  const ctx = dashboardContext();
  let stats = $state<Stats | null>(null);
  let error = $state<string | null>(null);
  /** Every app function's executions over the last 24 hours, in 15-minute buckets (refreshed every 30s). */
  let series = $state<Timeseries | null>(null);
  /** Recent samples of the live counters (one per poll, 5 minutes' worth), for the live charts. */
  let history = $state<Sample[]>([]);
  let previous: Stats | null = null;

  interface Sample {
    at: number;
    commits: number;
    reruns: number;
    sockets: number;
    subscriptions: number;
    shared: number;
    pending: number;
    running: number;
    /** Percent of the cache lookups since the previous poll that hit (null without lookups). */
    hitRate: number | null;
  }

  const DAY = RANGES.find((r) => r.id === "24h")!;

  onMount(() => {
    let stopped = false;
    async function poll() {
      try {
        const next = await fetchStats(ctx.auth);
        const o = next.overview;
        const p = previous?.overview;
        // Reruns per second between two polls (the server reports a running total).
        const reruns =
          p && o.queryReruns !== undefined && p.queryReruns !== undefined
            ? Math.max(0, o.queryReruns - p.queryReruns) / Math.max(0.001, (o.now - p.now) / 1000)
            : 0;
        const hits = p ? (o.cacheHits ?? 0) - (p.cacheHits ?? 0) : 0;
        const lookups = p ? hits + (o.cacheMisses ?? 0) - (p.cacheMisses ?? 0) : 0;
        const sample: Sample = {
          at: o.now,
          commits: o.commitsPerSecond ?? 0,
          reruns,
          sockets: o.sockets,
          subscriptions: o.subscriptions,
          shared: o.sharedQueries ?? 0,
          pending: o.queueDepth ?? 0,
          running: o.runningJobs ?? 0,
          hitRate: lookups > 0 ? (Math.max(0, hits) / lookups) * 100 : null,
        };
        history = [...history, sample].slice(-100);
        previous = next;
        stats = next;
        error = null;
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
      if (!stopped) timer = setTimeout(poll, 3000);
    }
    async function pollSeries() {
      try {
        const { since, until } = rangeWindow(DAY.rangeMs, DAY.bucketMs, Date.now());
        const s = await fetchTimeseries(ctx.auth, { bucket: DAY.bucketMs, since, until });
        if (!stopped) series = s;
      } catch {
        // The stats poll reports a failing server; the charts keep their last data.
      }
      if (!stopped) seriesTimer = setTimeout(pollSeries, 30_000);
    }
    let timer = setTimeout(poll, 0);
    let seriesTimer = setTimeout(pollSeries, 0);
    return () => {
      stopped = true;
      clearTimeout(timer);
      clearTimeout(seriesTimer);
    };
  });

  const appFunctions = $derived((stats?.functions ?? []).filter((f) => !f.path.startsWith("_system")));
  const byCalls = $derived([...appFunctions].sort((a, b) => b.calls - a.calls).slice(0, 8));
  const byErrors = $derived(
    [...appFunctions]
      .filter((f) => f.errors > 0)
      .sort((a, b) => b.errors - a.errors)
      .slice(0, 8),
  );
  const appTables = $derived((ctx.tables ?? []).filter((t) => !t.system));
  const totalBytes = $derived((stats?.overview.files ?? []).reduce((n, f) => n + f.bytes, 0));

  const points = $derived(series ? fillBuckets(series) : []);
  const pointTimes = $derived(points.map((p) => p.t));
  const sum = $derived(totals(points));
  const times = $derived(history.map((h) => h.at));
  const pick = (f: (h: Sample) => number | null) => history.map(f);

  function cacheHitRate(o: Stats["overview"]): string {
    const total = (o.cacheHits ?? 0) + (o.cacheMisses ?? 0);
    return total === 0 ? "–" : `${Math.round(((o.cacheHits ?? 0) / total) * 100)}%`;
  }

  const rate = (n: number) => (n >= 100 ? n.toFixed(0) : n >= 10 ? n.toFixed(1) : n.toFixed(2));
  const count = (n: number) => (n >= 10_000 ? `${Number((n / 1000).toFixed(0))}k` : n >= 1000 ? `${Number((n / 1000).toFixed(1))}k` : `${Number(n.toFixed(2))}`);
  const percent = (n: number) => `${Number(n.toFixed(n < 1 ? 2 : n < 10 ? 1 : 0))}%`;
  const ms = (n: number) => (n >= 1000 ? `${Number((n / 1000).toFixed(1))}s` : n >= 10 ? `${Math.round(n)}ms` : `${Number(n.toFixed(2))}ms`);
  const live = "Collecting samples…";
</script>

{#snippet section(title: string, note: string, body: Snippet)}
  <details class="section" open>
    <summary><span class="chevron" aria-hidden="true"></span>{title}<span class="note faint">{note}</span></summary>
    {@render body()}
  </details>
{/snippet}

{#snippet stat(value: string | number, sub: string, testid?: string)}
  <div class="value" data-testid={testid}>{value}</div>
  <div class="sub">{sub}</div>
{/snippet}

{#snippet headline(value: string, sub: string, testid?: string)}
  <div class="headline"><span class="value" data-testid={testid}>{value}</span><span class="sub">{sub}</span></div>
{/snippet}

{#snippet fnTable(rows: FunctionStats[], empty: string)}
  {#if rows.length === 0}
    <p class="muted small">{empty}</p>
  {:else}
    <table class="grid">
      <thead><tr><th>Function</th><th>Calls</th><th>Errors</th><th>p50</th><th>p95</th></tr></thead>
      <tbody>
        {#each rows as f (f.path)}
          <tr>
            <td><a href={href("functions", [], { fn: f.path })} onclick={link} class="mono">{f.path}</a></td>
            <td>{f.calls}</td>
            <td class:error-text={f.errors > 0}>{f.errors}</td>
            <td>{formatDuration(f.p50Us)}</td>
            <td>{formatDuration(f.p95Us)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
{/snippet}

<div class="page">
  <header class="page-header">
    <h1>Overview</h1>
    {#if stats}<span class="muted small">rxferrix {stats.overview.version}{stats.overview.dev ? " · dev mode" : ""}</span>{/if}
    <span class="spacer"></span>
    <span class="faint small">refreshes every 3s</span>
  </header>
  <div class="page-body">
    {#if error}<div class="alert" role="alert">{error}</div>{/if}
    {#if !stats}
      {#if !error}<Empty title="Loading stats…" />{/if}
    {:else}
      {@const o = stats.overview}

      {#snippet statusBody()}
        <div class="tiles">
          <Panel title="Commit ts" help="The timestamp of the latest commit, and when it happened.">
            {@render stat(o.ts, formatTime(o.commitTime), "stat-ts")}
          </Panel>
          <Panel title="Connections" help="Open WebSocket connections, and the live query subscriptions they hold.">
            {@render stat(o.sockets, `${o.subscriptions} live subscriptions`, "stat-sockets")}
          </Panel>
          <Panel title="Documents" help="Documents in every table, system tables included.">
            {@render stat(o.documents, `in ${o.tables} tables`)}
          </Panel>
          <Panel title="Functions" help="Queries, mutations and actions the app registers.">
            {@render stat(o.registeredFunctions, "registered")}
          </Panel>
          <Panel title="Storage" help="data.db and aux.db on disk, write-ahead logs included.">
            {@render stat(formatBytes(totalBytes), "data.db + aux.db (with WAL)")}
          </Panel>
          <Panel title="Uptime" help="How long this server process has been running.">
            {@render stat(formatUptime(o.now - o.startedAt), `since ${formatTime(o.startedAt)}`)}
          </Panel>
          <Panel
            title="Dropped log entries"
            help="Execution log entries dropped because the writer fell behind; non-zero means the log is incomplete."
            warn={o.droppedLogEntries > 0}
          >
            {@render stat(o.droppedLogEntries, "execution log backlog")}
          </Panel>
          {#if stats!.olap}
            {@const a = stats!.olap}
            <Panel title="Analytics copy" help={a.detail ?? "The analytics copy of the data (--olap)."} warn={a.state === "failed"} testid="stat-olap">
              <div class="value small-value">{a.state}</div>
              <div class="sub">
                {a.latestTs - a.appliedTs} commits behind{a.lagMs > 0 ? ` (${formatSpan(a.lagMs)})` : ""} ·
                {a.tables.reduce((n, t) => n + t.rows, 0)} rows · {formatBytes(a.sizeBytes)}
              </div>
            </Panel>
          {/if}
          <Panel title="Prometheus" help="Whether the server exposes Prometheus metrics at /metrics.">
            <div class="value small-value">{o.metricsEnabled ? "on" : "off"}</div>
            <div class="sub">{o.metricsEnabled ? "/metrics is served" : "start with --metrics"}</div>
          </Panel>
        </div>
      {/snippet}
      {@render section("Status", "", statusBody)}

      {#snippet functionsBody()}
        <div class="charts">
          <Panel title="Function Calls" help="Calls of every app function per 15 minutes, and how many of them failed.">
            {#snippet actions()}<span class="total">{count(sum.calls)} calls</span>{/snippet}
            <LineChart
              label="Function calls"
              times={pointTimes}
              step={series?.bucket ?? 0}
              format={count}
              empty="No function calls yet."
              series={[
                { label: "Calls", color: "var(--series-1)", values: points.map((p) => p.calls) },
                { label: "Errors", color: "var(--err)", values: points.map((p) => p.errors) },
              ]}
            />
          </Panel>
          <Panel title="Failure Rate" help="The share of app function calls that threw (user and server errors), per 15 minutes.">
            {#snippet actions()}<span class="total" class:error-text={sum.errors > 0}>{percent(sum.errorRate * 100)} overall</span>{/snippet}
            <LineChart
              label="Failure rate"
              times={pointTimes}
              step={series?.bucket ?? 0}
              format={percent}
              min={100}
              max={100}
              empty="No function calls yet."
              series={[{ label: "All functions", color: "var(--err)", values: points.map((p) => (p.calls ? (p.errors / p.calls) * 100 : null)) }]}
            />
          </Panel>
          <Panel title="Latency" help="The median (p50) and 95th percentile (p95) duration of app function calls, per 15 minutes.">
            {#snippet actions()}{#if sum.worstP95 !== null}<span class="total">worst p95 {ms(sum.worstP95)}</span>{/if}{/snippet}
            <LineChart
              label="Latency"
              times={pointTimes}
              step={series?.bucket ?? 0}
              format={ms}
              empty="No function calls yet."
              series={[
                { label: "p50", color: "var(--series-1)", values: points.map((p) => p.p50) },
                { label: "p95", color: "var(--series-2)", values: points.map((p) => p.p95) },
              ]}
            />
          </Panel>
        </div>
        <div class="tables">
          <Panel title="Busiest functions" help="The app functions called most in the last 24 hours.">
            {@render fnTable(byCalls, "No function calls in the last 24 hours.")}
          </Panel>
          <Panel title="Most errors" help="The app functions that failed most in the last 24 hours.">
            {@render fnTable(byErrors, "No errors in the last 24 hours.")}
          </Panel>
        </div>
      {/snippet}
      {@render section("Functions", "last 24 hours", functionsBody)}

      {#if o.commitsPerSecond !== undefined}
        {#snippet liveBody()}
          <div class="charts" data-testid="live-stats">
            <Panel title="Commits / s" help="Write transactions committed per second, averaged over the last 10 seconds.">
              {@render headline(rate(o.commitsPerSecond!), "over the last 10 seconds", "stat-commits")}
              <LineChart label="Commits per second" {times} format={count} empty={live} series={[{ label: "Commits / s", color: "var(--series-1)", values: pick((h) => h.commits) }]} />
            </Panel>
            <Panel title="Query reruns / s" help="Subscribed queries rerun per second because a commit changed what they read.">
              {@render headline(rate(history.at(-1)?.reruns ?? 0), `${o.queryReruns ?? 0} reruns in ${o.subscriptionRounds ?? 0} rounds`)}
              <LineChart label="Query reruns per second" {times} format={count} empty={live} series={[{ label: "Reruns / s", color: "var(--series-1)", values: pick((h) => h.reruns) }]} />
            </Panel>
            <Panel title="Subscriptions" help="Open connections, the live subscriptions they hold, and the distinct (shared) queries serving them.">
              {@render headline(String(o.sharedQueries ?? 0), `shared queries serving ${o.subscriptions} subscriptions · ${o.cachedQueries ?? 0} cached`)}
              <LineChart
                label="Subscriptions"
                {times}
                format={count}
                empty={live}
                series={[
                  { label: "Connections", color: "var(--series-1)", values: pick((h) => h.sockets) },
                  { label: "Subscriptions", color: "var(--series-2)", values: pick((h) => h.subscriptions) },
                  { label: "Shared queries", color: "var(--series-3)", values: pick((h) => h.shared) },
                ]}
              />
            </Panel>
            <Panel title="Query Cache" help="HTTP queries and ctx.run_query calls answered from the cache, as a share of all lookups: since the server started in the headline, per 3-second sample in the chart." testid="stat-cache">
              {@render headline(cacheHitRate(o), `hit rate since start · ${formatBytes(o.cacheBytes ?? 0)} kept`)}
              <LineChart
                label="Query cache hit rate"
                {times}
                format={percent}
                min={100}
                max={100}
                empty={times.length < 2 ? live : "No cache lookups in the last 5 minutes."}
                series={[{ label: "Hit rate", color: "var(--series-3)", values: pick((h) => h.hitRate) }]}
              />
            </Panel>
            <Panel title="Scheduler" help="Scheduled jobs waiting to run, and scheduled actions in progress." href={href("scheduler", [], { state: "pending" })} onclick={link}>
              {@render headline(String(o.queueDepth ?? 0), `pending jobs · ${o.runningJobs ?? 0} running`, "stat-queue")}
              <LineChart
                label="Scheduler queue"
                {times}
                format={count}
                empty={live}
                series={[
                  { label: "Pending", color: "var(--series-1)", values: pick((h) => h.pending) },
                  { label: "Running", color: "var(--series-2)", values: pick((h) => h.running) },
                ]}
              />
            </Panel>
          </div>
        {/snippet}
        {@render section("Live", "sampled every 3s · last 5 minutes", liveBody)}
      {/if}

      {#snippet storageBody()}
        <div class="tables">
          <Panel title="Tables" help="The app's tables, their documents and indexes.">
            {#if appTables.length === 0}
              <p class="muted small">No app tables yet.</p>
            {:else}
              <table class="grid">
                <thead><tr><th>Table</th><th>Documents</th><th>Indexes</th></tr></thead>
                <tbody>
                  {#each appTables as t (t.name)}
                    <tr>
                      <td><a href={href("data", [t.name])} onclick={link}>{t.name}</a></td>
                      <td>{t.documents}</td>
                      <td class="muted">{t.indexes.filter((i) => !i.builtin).map((i) => i.name).join(", ") || "—"}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            {/if}
          </Panel>
          <Panel title="Files" help="The database files on disk.">
            <table class="grid">
              <tbody>
                {#each o.files as f (f.name)}
                  <tr><td class="mono">{f.name}</td><td>{formatBytes(f.bytes)}</td></tr>
                {/each}
              </tbody>
            </table>
          </Panel>
        </div>
      {/snippet}
      {@render section("Storage", "", storageBody)}
    {/if}
  </div>
</div>

<style>
  .small {
    font-size: 12.5px;
  }
  .section {
    margin-bottom: 18px;
  }
  summary {
    display: flex;
    align-items: center;
    gap: 8px;
    width: fit-content;
    margin-bottom: 10px;
    font-size: 15px;
    font-weight: 600;
    cursor: pointer;
    list-style: none;
    user-select: none;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  .chevron {
    width: 7px;
    height: 7px;
    margin: 0 3px;
    border-right: 1.5px solid var(--muted);
    border-bottom: 1.5px solid var(--muted);
    transform: rotate(-45deg);
    transition: transform 0.12s;
  }
  details[open] > summary .chevron {
    transform: rotate(45deg) translate(-2px, -2px);
  }
  .note {
    font-size: 12px;
    font-weight: 400;
  }
  .tiles {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
    gap: 12px;
  }
  .charts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(340px, 1fr));
    gap: 12px;
  }
  .tables {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(380px, 1fr));
    gap: 12px;
    margin-top: 12px;
  }
  .section > .tables:first-of-type {
    margin-top: 0;
  }
  .tables :global(.body) {
    overflow: auto;
  }
  .value {
    font-size: 22px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .small-value {
    font-size: 18px;
  }
  .sub {
    font-size: 12px;
    color: var(--faint);
  }
  .headline {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin-bottom: 6px;
    min-width: 0;
  }
  .headline .value {
    font-size: 20px;
  }
  .headline .sub {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .total {
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
  table.grid tbody tr {
    cursor: default;
  }
</style>
