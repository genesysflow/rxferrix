<script lang="ts">
  import type { Timeseries } from "../lib/api.js";
  import { fetchTimeseries } from "../lib/admin.js";
  import { fillBuckets, RANGES, rangeWindow, totals, type RangeId } from "../lib/chart.js";
  import { dashboardContext } from "../lib/context.js";
  import { formatDuration } from "../lib/format.js";
  import TimeChart from "./TimeChart.svelte";

  /** Per-function latency (p50/p95) and calls/errors over time, from the execution log; refreshes every 10s. */
  let { path }: { path: string } = $props();
  const ctx = dashboardContext();

  let rangeId = $state<RangeId>("1h");
  const range = $derived(RANGES.find((r) => r.id === rangeId) ?? RANGES[0]);
  let series = $state<Timeseries | null>(null);
  let error = $state<string | null>(null);

  $effect(() => {
    const { rangeMs, bucketMs } = range;
    const p = path;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    series = null;
    error = null;
    async function poll() {
      try {
        const { since, until } = rangeWindow(rangeMs, bucketMs, Date.now());
        const s = await fetchTimeseries(ctx.auth, { path: p, bucket: bucketMs, since, until });
        if (!stopped) {
          series = s;
          error = null;
        }
      } catch (e) {
        if (!stopped) error = e instanceof Error ? e.message : String(e);
      }
      if (!stopped) timer = setTimeout(poll, 10_000);
    }
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  });

  const points = $derived(series ? fillBuckets(series) : []);
  const sum = $derived(totals(points));
  const p50s = $derived(
    series && series.points.length
      ? [...series.points].sort((a, b) => a.p50Us - b.p50Us)[Math.floor(series.points.length / 2)]!.p50Us
      : null,
  );
</script>

<section class="activity" aria-label="Activity">
  <div class="head">
    <h3>Activity</h3>
    <div class="ranges" role="group" aria-label="Time range">
      {#each RANGES as r (r.id)}
        <button class="small" class:active={r.id === rangeId} aria-pressed={r.id === rangeId} onclick={() => (rangeId = r.id)}>{r.id}</button>
      {/each}
    </div>
    <span class="spacer"></span>
    {#if series}
      <span class="stats" data-testid="activity-totals">
        <span><strong>{sum.calls}</strong> calls</span>
        <span class:error-text={sum.errors > 0}><strong>{sum.errors}</strong> errors{#if sum.calls}&nbsp;({(sum.errorRate * 100).toFixed(sum.errorRate < 0.1 ? 1 : 0)}%){/if}</span>
        {#if p50s !== null}<span>typical p50 <strong>{formatDuration(p50s)}</strong></span>{/if}
        {#if sum.worstP95 !== null}<span>worst p95 <strong>{formatDuration(sum.worstP95 * 1000)}</strong></span>{/if}
      </span>
    {/if}
  </div>
  {#if error}
    <div class="alert" role="alert">{error}</div>
  {:else if !series}
    <p class="faint">Loading…</p>
  {:else}
    {#if sum.calls === 0}
      <p class="faint empty">No calls in the last {range.label}. Run the function to see its latency here.</p>
    {/if}
    <div class="charts">
      <TimeChart {points} bucket={series.bucket} variant="latency" label="Latency" />
      <TimeChart {points} bucket={series.bucket} variant="volume" label="Calls and errors" />
    </div>
    <p class="faint note">
      {range.label} in {series.bucket >= 3_600_000 ? `${series.bucket / 3_600_000}-hour` : `${series.bucket / 60_000}-minute`} buckets, from
      the execution log (kept 7 days by default). Hover for the values.
    </p>
  {/if}
</section>

<style>
  .head {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    margin: 16px 0 6px;
  }
  .head h3 {
    margin: 0;
  }
  .ranges {
    display: inline-flex;
    gap: 2px;
  }
  .ranges button.active {
    background: var(--accent-soft);
    color: var(--accent-text);
    border-color: transparent;
    font-weight: 600;
  }
  .stats {
    display: inline-flex;
    gap: 14px;
    font-size: 12.5px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }
  .stats strong {
    color: var(--text);
  }
  .stats .error-text strong {
    color: var(--err);
  }
  .charts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
    gap: 18px;
    padding: 12px 14px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  .empty {
    margin: 0 0 8px;
  }
  .note {
    font-size: 12px;
    margin: 6px 0 0;
  }
</style>
