<script lang="ts">
  import type { LogEntry, LogFilter } from "../lib/api.js";
  import { fetchLogs, tailLogs } from "../lib/admin.js";
  import { dashboardContext } from "../lib/context.js";
  import { FLASH_MS, flash } from "../lib/flash.js";
  import { formatClock, formatDuration, formatTime, prettyArgs } from "../lib/format.js";
  import Empty from "./Empty.svelte";

  /**
   * Function executions matching `filter`, newest first: the recent history, then the live tail (entries that arrive
   * slide in, tinted by outcome). Click a row for its args, error and log lines; "Load older" pages back. Used by the
   * Logs page and, for one function, the function view.
   */
  let {
    filter,
    paused = false,
    pageSize = 100,
    showPath = true,
    status = $bindable<"connecting" | "live" | "error">("connecting"),
    statusError = $bindable<string | null>(null),
    lagged = $bindable(0),
  }: {
    filter: LogFilter;
    paused?: boolean;
    /** Entries loaded at first and per "Load older". */
    pageSize?: number;
    /** The Function column (off when every entry is the same function). */
    showPath?: boolean;
    status?: "connecting" | "live" | "error";
    statusError?: string | null;
    lagged?: number;
  } = $props();

  const ctx = dashboardContext();
  const MAX = 1000;
  /** A filter that changes while being typed restarts the tail once typing pauses. */
  const RESTART_DEBOUNCE_MS = 250;

  let entries = $state<LogEntry[]>([]);
  let expanded = $state<Set<number>>(new Set());
  let olderDone = $state(false);
  /** Entries kept: `MAX`, plus a page per "Load older". */
  let cap = MAX;
  /** Entries that arrived on the live tail (not history), and when: they animate in. */
  let arrived = $state.raw(new Map<number, number>());

  /** Merge entries, newest first, unique by id. */
  function merge(incoming: LogEntry[]) {
    const byId = new Map<number, LogEntry>();
    for (const e of entries) byId.set(e.id, e);
    for (const e of incoming) byId.set(e.id, e);
    entries = [...byId.values()].sort((a, b) => b.id - a.id).slice(0, cap);
  }

  function live(e: LogEntry) {
    if (!entries.some((x) => x.id === e.id)) {
      const now = performance.now();
      arrived = new Map([...[...arrived].filter(([, at]) => now - at < FLASH_MS), [e.id, now]]);
    }
    merge([e]);
  }

  // (Re)start the tail whenever the filter changes or the view is resumed: open the stream first, then fill in the
  // history, so nothing between the two is missed (duplicates are merged by id).
  // Keyed by value: a filter built inline by the parent is a new object on every render.
  const filterKey = $derived(JSON.stringify(filter));
  let started = false;
  $effect(() => {
    const key = filterKey;
    const f = JSON.parse(key) as LogFilter;
    if (paused) return;
    const abort = new AbortController();
    entries = [];
    arrived = new Map();
    olderDone = false;
    cap = MAX;
    const start = () =>
      void tailLogs(
        ctx.auth,
        f,
        {
          onReady: () => {
            // History for a filter that changed since belongs to that filter only.
            fetchLogs(ctx.auth, { ...f, limit: pageSize }).then(
              (history) => !abort.signal.aborted && merge(history),
              (e: unknown) => !abort.signal.aborted && (statusError = String(e)),
            );
          },
          onEntry: live,
          onLagged: (n) => (lagged += n),
          onStatus: (s, err) => {
            status = s;
            statusError = err ?? null;
          },
        },
        abort.signal,
      );
    // The first start is immediate; a filter being typed restarts once typing pauses.
    const timer = started ? setTimeout(start, RESTART_DEBOUNCE_MS) : (start(), undefined);
    started = true;
    return () => {
      clearTimeout(timer);
      abort.abort();
    };
  });

  async function loadOlder() {
    const last = entries.at(-1);
    if (!last) return;
    const key = filterKey;
    const older = await fetchLogs(ctx.auth, { ...filter, limit: pageSize, beforeId: last.id });
    if (key !== filterKey) return;
    if (older.length < pageSize) olderDone = true;
    cap = Math.max(cap, entries.length + older.length);
    merge(older);
  }

  function toggle(id: number) {
    const next = new Set(expanded);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    expanded = next;
  }

  const outcomeColor = { success: "--ok", userError: "--warn", serverError: "--err" } as const;
  const columns = $derived(showPath ? 6 : 5);
</script>

{#if entries.length === 0}
  <Empty
    title={paused
      ? "Paused"
      : status !== "live"
        ? "Connecting…"
        : filter.minDurationUs
          ? `No executions took ${formatDuration(filter.minDurationUs)} or longer`
          : "No executions yet"}
    hint={filter.minDurationUs
      ? "Slower executions appear here live as they happen; choose a shorter duration to see more."
      : "Function executions appear here live as they happen."}
  />
{:else}
  <table class="grid" data-testid="logs">
    <thead>
      <tr>
        <th>Time</th><th>Kind</th>{#if showPath}<th>Function</th>{/if}<th>Outcome</th><th>Duration</th><th>Caller</th>
      </tr>
    </thead>
    <tbody>
      {#each entries as e (e.id)}
        {@const at = arrived.get(e.id)}
        <tr
          onclick={() => toggle(e.id)}
          class:selected={expanded.has(e.id)}
          aria-expanded={expanded.has(e.id)}
          use:flash={at === undefined ? undefined : { kind: "inserted", at, color: outcomeColor[e.outcome] }}
        >
          <td class="mono" title={formatTime(e.timestamp)}>{formatClock(e.timestamp)}</td>
          <td><span class="badge {e.kind}">{e.kind}</span></td>
          {#if showPath}<td class="mono">{e.path}</td>{/if}
          <td><span class="badge {e.outcome}">{e.outcome}</span></td>
          <td class="num">{formatDuration(e.durationUs)}</td>
          <td class="mono faint">{e.identity ?? "anonymous"}</td>
        </tr>
        {#if expanded.has(e.id)}
          <tr class="detail">
            <td colspan={columns}>
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

<style>
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
