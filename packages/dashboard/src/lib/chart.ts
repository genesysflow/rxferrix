/**
 * Data transforms for the Functions page's charts (unit-tested in test/chart.test.ts): the server returns only the
 * time buckets that had executions; the charts want every bucket of the range, with zero calls and no latency for
 * empty ones (a gap in the latency lines, not a drop to 0), plus axis ticks and SVG paths.
 */
import type { TimePoint, Timeseries } from "./api.js";

export interface ChartPoint {
  /** The bucket's start (ms since the epoch). */
  t: number;
  calls: number;
  errors: number;
  /** Milliseconds; null for a bucket without executions. */
  p50: number | null;
  p95: number | null;
}

/** Every bucket from `series.since` up to `series.until`, oldest first; at most `maxBuckets`. */
export function fillBuckets(series: Timeseries, maxBuckets = 2000): ChartPoint[] {
  const { bucket, since, until } = series;
  if (!(bucket > 0) || !(until > since)) return [];
  const byT = new Map<number, TimePoint>(series.points.map((p) => [p.t, p]));
  const out: ChartPoint[] = [];
  for (let t = since; t < until && out.length < maxBuckets; t += bucket) {
    const p = byT.get(t);
    out.push(
      p
        ? { t, calls: p.calls, errors: p.errors, p50: p.p50Us / 1000, p95: p.p95Us / 1000 }
        : { t, calls: 0, errors: 0, p50: null, p95: null },
    );
  }
  return out;
}

export interface Totals {
  calls: number;
  errors: number;
  /** Errors / calls (0 without calls). */
  errorRate: number;
  /** The slowest bucket's p95, ms (null without calls). */
  worstP95: number | null;
}

export function totals(points: readonly ChartPoint[]): Totals {
  let calls = 0;
  let errors = 0;
  let worstP95: number | null = null;
  for (const p of points) {
    calls += p.calls;
    errors += p.errors;
    if (p.p95 !== null && (worstP95 === null || p.p95 > worstP95)) worstP95 = p.p95;
  }
  return { calls, errors, errorRate: calls ? errors / calls : 0, worstP95 };
}

/**
 * About `count` round tick values from 0 to at least `max` (1, 2 or 5 × 10^n steps). `whole` keeps the steps whole
 * numbers, for counts (no "0.5 calls").
 */
export function niceTicks(max: number, count = 4, whole = false): number[] {
  if (!(max > 0) || !Number.isFinite(max)) return [0, 1];
  const raw = max / count;
  const power = 10 ** Math.floor(Math.log10(raw));
  const steps = [1, 2, 2.5, 5, 10].map((m) => m * power).filter((s) => !whole || (s >= 1 && Number.isInteger(s)));
  const step = steps.find((s) => s >= raw) ?? (whole ? Math.max(1, 10 * power) : 10 * power);
  const ticks: number[] = [];
  for (let v = 0; v < max + step * 1e-9; v += step) ticks.push(roundTo(v, step));
  if (ticks[ticks.length - 1]! < max) ticks.push(roundTo(ticks[ticks.length - 1]! + step, step));
  return ticks;
}

function roundTo(v: number, step: number): number {
  const decimals = Math.max(0, -Math.floor(Math.log10(step)) + 1);
  return Number(v.toFixed(decimals));
}

/** A linear map from `[d0, d1]` to `[r0, r1]`. */
export function scale(d0: number, d1: number, r0: number, r1: number): (v: number) => number {
  const span = d1 - d0 || 1;
  return (v) => r0 + ((v - d0) / span) * (r1 - r0);
}

/** An SVG path through the points, broken where the value is null (one `M` per run). */
export function linePath<P extends { t: number } = ChartPoint>(
  points: readonly NoInfer<P>[],
  value: (p: P) => number | null,
  x: (t: number) => number,
  y: (v: number) => number,
): string {
  let d = "";
  let pen = false;
  for (const p of points) {
    const v = value(p);
    if (v === null) {
      pen = false;
      continue;
    }
    d += `${pen ? "L" : "M"}${x(p.t).toFixed(1)},${y(v).toFixed(1)}`;
    pen = true;
  }
  return d;
}

/** The index of the point whose bucket contains (or is nearest to) the x position `px`. */
export function nearestIndex(points: readonly { t: number }[], px: number, x: (t: number) => number, bucket: number): number {
  if (points.length === 0) return -1;
  let best = 0;
  let bestDistance = Infinity;
  for (let i = 0; i < points.length; i++) {
    const center = x(points[i]!.t + bucket / 2);
    const distance = Math.abs(center - px);
    if (distance < bestDistance) {
      best = i;
      bestDistance = distance;
    }
  }
  return best;
}

/** Axis labels for a time: `HH:MM:SS` within ten minutes, `HH:MM` within a day's range, else `MM-DD HH:MM`. */
export function timeLabel(t: number, rangeMs: number): string {
  const d = new Date(t);
  const pad = (n: number) => String(n).padStart(2, "0");
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (rangeMs < 600_000) return `${hm}:${pad(d.getSeconds())}`;
  return rangeMs > 86_400_000 ? `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${hm}` : hm;
}

const TIME_STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400].map((s) => s * 1000);

/**
 * Round times in `[t0, t1]` for an axis (whole seconds, minutes or hours in local time): the smallest step that
 * gives at most `maxCount` ticks.
 */
export function timeTicks(t0: number, t1: number, maxCount: number): number[] {
  if (!(t1 > t0) || maxCount < 1) return [];
  const step = TIME_STEPS.find((s) => (t1 - t0) / s <= maxCount) ?? Math.ceil((t1 - t0) / maxCount / 86_400_000) * 86_400_000;
  // Align to the local day (steps of hours land on 00:00, 06:00, ... rather than on UTC hours).
  const offset = new Date(t0).getTimezoneOffset() * 60_000;
  const out: number[] = [];
  for (let t = Math.ceil((t0 - offset) / step) * step + offset; t <= t1; t += step) out.push(t);
  return out;
}

/** The time ranges the charts offer, with a bucket size that gives 60 to 168 bars. */
export const RANGES = [
  { id: "1h", label: "1 hour", rangeMs: 3_600_000, bucketMs: 60_000 },
  { id: "6h", label: "6 hours", rangeMs: 6 * 3_600_000, bucketMs: 5 * 60_000 },
  { id: "24h", label: "24 hours", rangeMs: 24 * 3_600_000, bucketMs: 15 * 60_000 },
  { id: "7d", label: "7 days", rangeMs: 7 * 24 * 3_600_000, bucketMs: 3_600_000 },
] as const;
export type RangeId = (typeof RANGES)[number]["id"];

/** `[since, until)` for a range ending now, aligned to whole buckets so the latest bucket is the current one. */
export function rangeWindow(rangeMs: number, bucketMs: number, now: number): { since: number; until: number } {
  const until = (Math.floor(now / bucketMs) + 1) * bucketMs;
  return { since: until - Math.ceil(rangeMs / bucketMs) * bucketMs, until };
}
