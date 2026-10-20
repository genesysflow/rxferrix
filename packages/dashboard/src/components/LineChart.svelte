<script lang="ts" module>
  export interface Series {
    label: string;
    /** A CSS color (a `--series-n` variable). */
    color: string;
    /** One value per time; null breaks the line. */
    values: (number | null)[];
  }
</script>

<script lang="ts">
  import { linePath, nearestIndex, niceTicks, scale, timeLabel, timeTicks } from "../lib/chart.js";

  /**
   * A small SVG line chart over time for the overview's cards: one line per series on one y axis, gridlines, time
   * labels, a legend below (two or more series), and a crosshair with one tooltip listing every series on hover.
   * `step` is the bucket width (ms) when each time starts a bucket (the value is drawn at its middle), 0 for samples.
   */
  let {
    times,
    series,
    step = 0,
    format = (v: number) => String(v),
    height = 140,
    min = 1,
    max,
    label,
    empty = "No data yet.",
  }: {
    times: number[];
    series: Series[];
    step?: number;
    format?: (v: number) => string;
    height?: number;
    /** The y axis reaches at least this. */
    min?: number;
    /** …and at most this (100 for a percentage). */
    max?: number;
    label: string;
    empty?: string;
  } = $props();

  let width = $state(320);
  let hover = $state<number | null>(null);
  const M = { top: 8, right: 6, bottom: 20, left: 40 };

  const pts = $derived(times.map((t, i) => ({ t, i })));
  const t0 = $derived(times[0] ?? 0);
  const t1 = $derived((times.at(-1) ?? 0) + step);
  const x = $derived(scale(t0, t1, M.left, Math.max(M.left + 10, width - M.right)));
  const peak = $derived(Math.max(min, ...series.flatMap((s) => s.values.map((v) => v ?? 0))));
  const ticks = $derived(niceTicks(max === undefined ? peak : Math.min(peak, max), 4));
  const y = $derived(scale(0, ticks.at(-1) ?? 1, height - M.bottom, M.top));
  /** Time labels at round times, skipping any that would hang over the plot's edges. */
  const xTicks = $derived(
    timeTicks(t0, t1, Math.max(2, Math.floor((width - M.left - M.right) / 80)))
      .filter((t) => x(t) >= M.left + 22 && x(t) <= width - M.right - 22)
      .map((t) => ({ t, text: timeLabel(t, t1 - t0) })),
  );
  const cx = (t: number) => x(t + step / 2);
  const paths = $derived(series.map((s) => linePath<{ t: number; i: number }>(pts, (p) => s.values[p.i] ?? null, cx, y)));

  function onmove(e: PointerEvent) {
    const px = e.clientX - (e.currentTarget as SVGSVGElement).getBoundingClientRect().left;
    hover = px < M.left || px > width - M.right ? null : nearestIndex(pts, px, x, step);
  }

  function clock(t: number) {
    const d = new Date(t);
    const pad = (n: number) => String(n).padStart(2, "0");
    return `${pad(d.getHours())}:${pad(d.getMinutes())}${step ? "" : `:${pad(d.getSeconds())}`}`;
  }
</script>

<figure class="chart" bind:clientWidth={width}>
  {#if times.length < 2}
    <div class="empty faint" style:height={`${height}px`}>{empty}</div>
  {:else}
    <svg {width} {height} role="img" aria-label={label} onpointermove={onmove} onpointerleave={() => (hover = null)}>
      {#each ticks as t (t)}
        <line class="grid" x1={M.left} x2={width - M.right} y1={y(t)} y2={y(t)} />
        <text class="axis" x={M.left - 6} y={y(t)} text-anchor="end" dominant-baseline="middle">{format(t)}</text>
      {/each}
      {#each xTicks as tick (tick.t)}
        <line class="tick" x1={x(tick.t)} x2={x(tick.t)} y1={y(0)} y2={y(0) + 3} />
        <text class="axis" x={x(tick.t)} y={height - 5} text-anchor="middle">{tick.text}</text>
      {/each}
      <line class="baseline" x1={M.left} x2={width - M.right} y1={y(0)} y2={y(0)} />
      {#each series as s, k (s.label)}
        <path class="series" d={paths[k]} style:stroke={s.color} />
        {#each s.values as v, i (i)}
          <!-- Isolated values (no neighbors) would be invisible as a line: draw a dot. -->
          {#if v !== null && (s.values[i - 1] ?? null) === null && (s.values[i + 1] ?? null) === null}
            <circle cx={cx(times[i]!)} cy={y(v)} r="2.5" style:fill={s.color} />
          {/if}
        {/each}
      {/each}
      {#if hover !== null}
        {@const hx = cx(times[hover]!)}
        <line class="crosshair" x1={hx} x2={hx} y1={M.top} y2={height - M.bottom} />
        {#each series as s (s.label)}
          {@const v = s.values[hover!] ?? null}
          {#if v !== null}<circle class="mark" cx={hx} cy={y(v)} r="4" style:fill={s.color} />{/if}
        {/each}
      {/if}
    </svg>
    {#if hover !== null}
      {@const hx = cx(times[hover]!)}
      <!-- Beside the crosshair, on the side with more room, so it covers as little of the lines as possible. -->
      <div
        class="tooltip"
        style:left={hx < width / 2 ? `${hx + 12}px` : undefined}
        style:right={hx < width / 2 ? undefined : `${width - hx + 12}px`}
        role="status"
      >
        <div class="when">{clock(times[hover]!)}{#if step} – {clock(times[hover]! + step)}{/if}</div>
        {#each series as s (s.label)}
          {@const v = s.values[hover!] ?? null}
          <div class="value"><span class="swatch" style:background={s.color}></span>{s.label}<strong>{v === null ? "—" : format(v)}</strong></div>
        {/each}
      </div>
    {/if}
  {/if}
  {#if series.length > 1}
    <figcaption>
      {#each series as s (s.label)}
        <span class="key"><span class="swatch" style:background={s.color}></span>{s.label}</span>
      {/each}
    </figcaption>
  {/if}
</figure>

<style>
  .chart {
    position: relative;
    margin: 0;
    min-width: 0;
  }
  .empty {
    display: grid;
    place-items: center;
    font-size: 12.5px;
    border: 1px dashed var(--border);
    border-radius: 6px;
  }
  svg {
    display: block;
    touch-action: none;
  }
  .grid {
    stroke: var(--border);
    stroke-width: 1;
  }
  .baseline {
    stroke: var(--border-strong);
    stroke-width: 1;
  }
  .axis {
    fill: var(--faint);
    font-size: 10.5px;
    font-family: var(--sans);
    font-variant-numeric: tabular-nums;
  }
  .series {
    fill: none;
    stroke-width: 2;
    stroke-linejoin: round;
    stroke-linecap: round;
  }
  .mark {
    stroke: var(--panel);
    stroke-width: 2;
  }
  .crosshair {
    stroke: var(--muted);
    stroke-width: 1;
    stroke-dasharray: 3 3;
    pointer-events: none;
  }
  figcaption {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 14px;
    margin-top: 8px;
    font-size: 12px;
    color: var(--muted);
  }
  .key {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .swatch {
    width: 10px;
    height: 2px;
    border-radius: 1px;
    flex: none;
  }
  .tick {
    stroke: var(--border-strong);
    stroke-width: 1;
  }
  .tooltip {
    position: absolute;
    top: 4px;
    width: max-content;
    background: var(--panel);
    border: 1px solid var(--border-strong);
    box-shadow: var(--shadow);
    border-radius: 6px;
    padding: 6px 9px;
    font-size: 12px;
    pointer-events: none;
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
    z-index: 2;
  }
  .value {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .value strong {
    margin-left: auto;
    padding-left: 12px;
  }
  .when {
    color: var(--muted);
  }
</style>
