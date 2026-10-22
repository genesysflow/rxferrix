<script lang="ts">
  import { linePath, nearestIndex, niceTicks, scale, timeLabel, type ChartPoint } from "../lib/chart.js";

  /**
   * A small SVG time chart over filled buckets (lib/chart.ts): `latency` draws p50 and p95 lines (ms; gaps where a
   * bucket had no calls), `volume` draws calls per bucket as bars with the errors stacked on top. Hovering shows a
   * crosshair and one tooltip with every value of that bucket.
   */
  let {
    points,
    bucket,
    variant,
    label,
    height = 180,
  }: { points: ChartPoint[]; bucket: number; variant: "latency" | "volume"; label: string; height?: number } = $props();

  let width = $state(600);
  let hover = $state<number | null>(null);
  const M = { top: 10, right: 12, bottom: 22, left: 46 };

  const t0 = $derived(points[0]?.t ?? 0);
  const t1 = $derived((points.at(-1)?.t ?? 0) + bucket);
  const x = $derived(scale(t0, t1, M.left, Math.max(M.left + 10, width - M.right)));
  const max = $derived(
    variant === "latency"
      ? Math.max(0, ...points.map((p) => p.p95 ?? 0))
      : Math.max(0, ...points.map((p) => p.calls)),
  );
  const ticks = $derived(niceTicks(max || (variant === "latency" ? 1 : 4), 4, variant === "volume"));
  const top = $derived(ticks.at(-1) ?? 1);
  const y = $derived(scale(0, top, height - M.bottom, M.top));
  const barWidth = $derived(Math.max(1, x(t0 + bucket) - x(t0) - 2));
  const xTicks = $derived.by(() => {
    const n = Math.max(2, Math.min(6, Math.floor(width / 110)));
    const out: number[] = [];
    for (let i = 0; i <= n; i++) out.push(t0 + ((t1 - t0) * i) / n);
    return out;
  });
  const p50Path = $derived(variant === "latency" ? linePath(points, (p) => p.p50, (t) => x(t + bucket / 2), y) : "");
  const p95Path = $derived(variant === "latency" ? linePath(points, (p) => p.p95, (t) => x(t + bucket / 2), y) : "");
  const hovered = $derived(hover === null ? null : (points[hover] ?? null));

  function fmtValue(v: number): string {
    if (variant === "volume") return Number.isInteger(v) ? String(v) : v.toFixed(1);
    return v >= 1000 ? `${(v / 1000).toFixed(v >= 10_000 ? 0 : 1)}s` : v >= 10 ? `${Math.round(v)}ms` : `${Number(v.toFixed(2))}ms`;
  }

  function ms(v: number | null): string {
    if (v === null) return "—";
    return v >= 1000 ? `${(v / 1000).toFixed(2)} s` : v >= 10 ? `${v.toFixed(0)} ms` : `${v.toFixed(2)} ms`;
  }

  function onmove(e: PointerEvent) {
    const svg = e.currentTarget as SVGSVGElement;
    const px = e.clientX - svg.getBoundingClientRect().left;
    hover = px < M.left || px > width - M.right ? null : nearestIndex(points, px, x, bucket);
  }

  function clock(t: number) {
    const d = new Date(t);
    const pad = (n: number) => String(n).padStart(2, "0");
    return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }
</script>

<figure class="chart" bind:clientWidth={width} data-testid={`chart-${variant}`}>
  <figcaption>
    <span class="title">{label}</span>
    <span class="legend">
      {#if variant === "latency"}
        <span class="key"><span class="swatch line s1"></span>p50</span>
        <span class="key"><span class="swatch line s2"></span>p95</span>
      {:else}
        <span class="key"><span class="swatch s1"></span>calls</span>
        <span class="key"><span class="swatch err"></span>errors</span>
      {/if}
    </span>
  </figcaption>
  <svg
    {width}
    {height}
    role="img"
    aria-label={label}
    onpointermove={onmove}
    onpointerleave={() => (hover = null)}
  >
    {#each ticks as t (t)}
      <line class="grid" x1={M.left} x2={width - M.right} y1={y(t)} y2={y(t)} />
      <text class="axis" x={M.left - 6} y={y(t)} text-anchor="end" dominant-baseline="middle">{fmtValue(t)}</text>
    {/each}
    {#each xTicks as t, i (i)}
      <text class="axis" x={x(t)} y={height - 6} text-anchor={i === 0 ? "start" : i === xTicks.length - 1 ? "end" : "middle"}
        >{timeLabel(t, t1 - t0)}</text
      >
    {/each}
    <line class="baseline" x1={M.left} x2={width - M.right} y1={y(0)} y2={y(0)} />

    {#if variant === "volume"}
      {#each points as p (p.t)}
        {#if p.calls > 0}
          {@const ok = p.calls - p.errors}
          {@const bx = x(p.t) + 1}
          {#if ok > 0}
            <rect class="bar s1" x={bx} width={barWidth} y={y(ok)} height={Math.max(1, y(0) - y(ok))} rx={Math.min(2, barWidth / 2)} />
          {/if}
          {#if p.errors > 0}
            <rect
              class="bar err"
              x={bx}
              width={barWidth}
              y={y(p.calls)}
              height={Math.max(1, y(ok) - y(p.calls) - (ok > 0 ? 1 : 0))}
              rx={Math.min(2, barWidth / 2)}
            />
          {/if}
        {/if}
      {/each}
    {:else}
      <path class="series s2" d={p95Path} />
      <path class="series s1" d={p50Path} />
      {#each points as p, i (p.t)}
        <!-- Isolated buckets (no neighbors) would be invisible as a line: draw a dot. -->
        {#if p.p50 !== null}
          {#if (points[i - 1]?.p50 ?? null) === null && (points[i + 1]?.p50 ?? null) === null}
            <circle class="dot s2" cx={x(p.t + bucket / 2)} cy={y(p.p95 ?? 0)} r="2.5" />
            <circle class="dot s1" cx={x(p.t + bucket / 2)} cy={y(p.p50)} r="2.5" />
          {/if}
        {/if}
      {/each}
    {/if}

    {#if hovered}
      {@const cx = x(hovered.t + bucket / 2)}
      <line class="crosshair" x1={cx} x2={cx} y1={M.top} y2={height - M.bottom} />
      {#if variant === "latency" && hovered.p50 !== null}
        <circle class="mark s2" cx={cx} cy={y(hovered.p95 ?? 0)} r="4" />
        <circle class="mark s1" cx={cx} cy={y(hovered.p50)} r="4" />
      {/if}
    {/if}
  </svg>
  {#if hovered}
    {@const left = x(hovered.t + bucket / 2)}
    <div class="tooltip" style:left={`${Math.min(Math.max(left, 90), width - 90)}px`} role="status">
      <div class="when">{clock(hovered.t)} – {clock(hovered.t + bucket).slice(6)}</div>
      <div><strong>{hovered.calls}</strong> calls{#if hovered.errors}, <strong class="error-text">{hovered.errors}</strong> errors{/if}</div>
      <div><strong>{ms(hovered.p50)}</strong> p50 · <strong>{ms(hovered.p95)}</strong> p95</div>
    </div>
  {/if}
</figure>

<style>
  .chart {
    position: relative;
    margin: 0;
    min-width: 0;
  }
  figcaption {
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 12.5px;
    margin-bottom: 4px;
  }
  .title {
    font-weight: 600;
  }
  .legend {
    display: inline-flex;
    gap: 12px;
    color: var(--muted);
  }
  .key {
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }
  .swatch {
    width: 10px;
    height: 10px;
    border-radius: 2px;
  }
  .swatch.line {
    height: 2px;
    border-radius: 1px;
  }
  .swatch.s1 {
    background: var(--series-1);
  }
  .swatch.s2 {
    background: var(--series-2);
  }
  .swatch.err {
    background: var(--err);
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
  .series.s1 {
    stroke: var(--series-1);
  }
  .series.s2 {
    stroke: var(--series-2);
  }
  .bar.s1,
  .dot.s1,
  .mark.s1 {
    fill: var(--series-1);
  }
  .dot.s2,
  .mark.s2 {
    fill: var(--series-2);
  }
  .mark {
    stroke: var(--panel);
    stroke-width: 2;
  }
  .bar.err {
    fill: var(--err);
  }
  .crosshair {
    stroke: var(--muted);
    stroke-width: 1;
    stroke-dasharray: 3 3;
    pointer-events: none;
  }
  .tooltip {
    position: absolute;
    top: 26px;
    transform: translateX(-50%);
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
  .when {
    color: var(--muted);
  }
</style>
