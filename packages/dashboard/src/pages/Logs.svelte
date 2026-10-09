<script lang="ts">
  import type { LogEntry, LogFilter } from "../lib/api.js";
  import { fetchLogs, tailLogs } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatClock, formatDuration, formatTime, prettyArgs } from "../lib/format.js";
  import Empty from "../components/Empty.svelte";

  const ctx = dashboardContext();
  const MAX = 1000;

  let pathPrefix = $state("");
  let kind = $state<LogFilter["kind"]>("");
  let outcome = $state<LogFilter["outcome"]>("");
  let includeSystem = $state(false);
  let paused = $state(false);
  let entries = $state<LogEntry[]>([]);
  let status = $state<"connecting" | "live" | "error">("connecting");
  let statusError = $state<string | null>(null);
  let lagged = $state(0);
  let expanded = $state<Set<number>>(new Set());
  let olderDone = $state(false);
  let pathInput: HTMLInputElement | undefined = $state();

  const filter = $derived<LogFilter>({
    pathPrefix: pathPrefix.trim() || undefined,
    kind,
    outcome,
    excludeSystem: !includeSystem,
  });

  /** Merge entries, newest first, unique by id. */
  function merge(incoming: LogEntry[]) {
    const byId = new Map<number, LogEntry>();
    for (const e of entries) byId.set(e.id, e);
    for (const e of incoming) byId.set(e.id, e);
    entries = [...byId.values()].sort((a, b) => b.id - a.id).slice(0, MAX);
  }

  // (Re)start the tail whenever the filter changes or the view is resumed: open the stream first, then fill in the
  // history, so nothing between the two is missed (duplicates are merged by id).
  $effect(() => {
    const f = { ...filter };
    if (paused) return;
    const abort = new AbortController();
    entries = [];
    olderDone = false;
    void tailLogs(
      ctx.auth,
      f,
      {
        onReady: () => {
          fetchLogs(ctx.auth, { ...f, limit: 100 }).then(merge, (e: unknown) => (statusError = String(e)));
        },
        onEntry: (e) => merge([e]),
        onLagged: (n) => (lagged += n),
        onStatus: (s, err) => {
          status = s;
          statusError = err ?? null;
        },
      },
      abort.signal,
    );
    return () => abort.abort();
  });

  async function loadOlder() {
    const last = entries.at(-1);
    if (!last) return;
    const older = await fetchLogs(ctx.auth, { ...filter, limit: 100, beforeId: last.id });
    if (older.length < 100) olderDone = true;
    merge(older);
  }

  function toggle(id: number) {
    const next = new Set(expanded);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    expanded = next;
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "/" && !(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)) {
      e.preventDefault();
      pathInput?.focus();
    }
  }
</script>

<svelte:window {onkeydown} />

<div class="page">
  <header class="page-header">
    <h1>Logs</h1>
    <span class="state" class:live={status === "live" && !paused} title={statusError ?? ""}>
      <span class="dot"></span>{paused ? "paused" : status === "live" ? "live" : status === "error" ? "reconnecting" : "connecting"}
    </span>
    {#if lagged > 0}<span class="badge userError" title="The tail fell behind; refresh to see everything">{lagged} skipped</span>{/if}
    <span class="spacer"></span>
    <button onclick={() => (paused = !paused)}>{paused ? "▶ Resume" : "❚❚ Pause"}</button>
  </header>
  <div class="filters row">
    <input bind:this={pathInput} class="mono" placeholder="Path prefix, e.g. todos:  ( / )" aria-label="Path prefix" bind:value={pathPrefix} />
    <select bind:value={kind} aria-label="Kind">
      <option value="">All kinds</option><option value="query">query</option><option value="mutation">mutation</option><option value="action">action</option>
    </select>
    <select bind:value={outcome} aria-label="Outcome">
      <option value="">All outcomes</option><option value="success">success</option><option value="userError">user error</option><option value="serverError">server error</option>
    </select>
    <label class="inline"><input type="checkbox" bind:checked={includeSystem} /> include system functions</label>
  </div>

  <div class="body">
    {#if entries.length === 0}
      <Empty title={status === "live" ? "No executions yet" : "Connecting…"} hint="Function executions appear here live as they happen." />
    {:else}
      <table class="grid" data-testid="logs">
        <thead><tr><th>Time</th><th>Kind</th><th>Function</th><th>Outcome</th><th>Duration</th><th>Caller</th></tr></thead>
        <tbody>
          {#each entries as e (e.id)}
            <tr onclick={() => toggle(e.id)} class:selected={expanded.has(e.id)} aria-expanded={expanded.has(e.id)}>
              <td class="mono" title={formatTime(e.timestamp)}>{formatClock(e.timestamp)}</td>
              <td><span class="badge {e.kind}">{e.kind}</span></td>
              <td class="mono">{e.path}</td>
              <td><span class="badge {e.outcome}">{e.outcome}</span></td>
              <td class="num">{formatDuration(e.durationUs)}</td>
              <td class="mono faint">{e.identity ?? "anonymous"}</td>
            </tr>
            {#if expanded.has(e.id)}
              <tr class="detail">
                <td colspan="6">
                  {#if e.error}<h3>Error</h3><pre class="err">{e.error}</pre>{/if}
                  <h3>Args{e.argsTruncated ? " (truncated)" : ""}</h3>
                  <pre>{prettyArgs(e.args)}</pre>
                  <h3>Log lines</h3>
                  {#if e.logLines.length === 0}<p class="faint">None.</p>{:else}<pre>{e.logLines.join("\n")}</pre>{/if}
                </td>
              </tr>
            {/if}
          {/each}
        </tbody>
      </table>
      <div class="more">
        {#if !olderDone}<button onclick={loadOlder}>Load older</button>{:else}<span class="faint">No older entries.</span>{/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .filters {
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .filters input.mono {
    width: 280px;
  }
  .body {
    flex: 1;
    overflow: auto;
  }
  .state {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
    font-size: 12.5px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--faint);
  }
  .state.live .dot {
    background: var(--ok);
  }
  .num {
    font-variant-numeric: tabular-nums;
  }
  tr.detail {
    cursor: default;
    background: var(--panel-2);
  }
  tr.detail:hover {
    background: var(--panel-2) !important;
  }
  tr.detail td {
    white-space: normal;
    max-width: none;
    padding: 4px 16px 12px;
  }
  tr.detail h3 {
    margin: 10px 0 4px;
  }
  pre.err {
    color: var(--err);
  }
  .more {
    padding: 12px 20px;
  }
</style>
