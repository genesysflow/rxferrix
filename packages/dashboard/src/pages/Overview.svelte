<script lang="ts">
  import { onMount } from "svelte";
  import type { FunctionStats, Stats } from "../lib/api.js";
  import { fetchStats } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatBytes, formatDuration, formatTime, formatUptime } from "../lib/format.js";
  import { href, link } from "../lib/router.svelte.js";
  import Empty from "../components/Empty.svelte";

  const ctx = dashboardContext();
  let stats = $state<Stats | null>(null);
  let error = $state<string | null>(null);

  onMount(() => {
    let stopped = false;
    async function poll() {
      try {
        stats = await fetchStats(ctx.auth);
        error = null;
      } catch (e) {
        error = e instanceof Error ? e.message : String(e);
      }
      if (!stopped) timer = setTimeout(poll, 3000);
    }
    let timer = setTimeout(poll, 0);
    return () => {
      stopped = true;
      clearTimeout(timer);
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
</script>

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
    {#if stats}<span class="muted small">ferrix {stats.overview.version}{stats.overview.dev ? " · dev mode" : ""}</span>{/if}
    <span class="spacer"></span>
    <span class="faint small">refreshes every 3s · last 24 hours</span>
  </header>
  <div class="page-body">
    {#if error}<div class="alert" role="alert">{error}</div>{/if}
    {#if !stats}
      {#if !error}<Empty title="Loading stats…" />{/if}
    {:else}
      {@const o = stats.overview}
      <section class="cards">
        <div class="card stat"><div class="label">Commit ts</div><div class="value" data-testid="stat-ts">{o.ts}</div><div class="sub">{formatTime(o.commitTime)}</div></div>
        <div class="card stat"><div class="label">Connections</div><div class="value" data-testid="stat-sockets">{o.sockets}</div><div class="sub">{o.subscriptions} live subscriptions</div></div>
        <div class="card stat"><div class="label">Documents</div><div class="value">{o.documents}</div><div class="sub">in {o.tables} tables</div></div>
        <div class="card stat"><div class="label">Functions</div><div class="value">{o.registeredFunctions}</div><div class="sub">registered</div></div>
        <div class="card stat"><div class="label">Storage</div><div class="value">{formatBytes(totalBytes)}</div><div class="sub">data.db + aux.db (with WAL)</div></div>
        <div class="card stat"><div class="label">Uptime</div><div class="value">{formatUptime(o.now - o.startedAt)}</div><div class="sub">since {formatTime(o.startedAt)}</div></div>
        <div class="card stat" class:warn={o.droppedLogEntries > 0}><div class="label">Dropped log entries</div><div class="value">{o.droppedLogEntries}</div><div class="sub">execution log backlog</div></div>
      </section>

      <section class="two">
        <div class="card pad">
          <h2>Busiest functions</h2>
          {@render fnTable(byCalls, "No function calls in the last 24 hours.")}
        </div>
        <div class="card pad">
          <h2>Most errors</h2>
          {@render fnTable(byErrors, "No errors in the last 24 hours.")}
        </div>
      </section>

      <section class="two">
        <div class="card pad">
          <h2>Tables</h2>
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
        </div>
        <div class="card pad">
          <h2>Files</h2>
          <table class="grid">
            <tbody>
              {#each o.files as f (f.name)}
                <tr><td class="mono">{f.name}</td><td>{formatBytes(f.bytes)}</td></tr>
              {/each}
            </tbody>
          </table>
        </div>
      </section>
    {/if}
  </div>
</div>

<style>
  .small {
    font-size: 12.5px;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(170px, 1fr));
    gap: 12px;
    margin-bottom: 16px;
  }
  .stat {
    padding: 12px 14px;
  }
  .stat.warn {
    border-color: var(--warn);
  }
  .label {
    font-size: 12px;
    color: var(--muted);
  }
  .value {
    font-size: 22px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .sub {
    font-size: 12px;
    color: var(--faint);
  }
  .two {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(380px, 1fr));
    gap: 16px;
    margin-bottom: 16px;
  }
  .pad {
    padding: 14px 16px;
    overflow: auto;
  }
  table.grid tbody tr {
    cursor: default;
  }
</style>
