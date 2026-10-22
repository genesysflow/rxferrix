import { describe, expect, it } from "vitest";
import type { TimePoint } from "../src/lib/api.js";
import { fillBuckets, linePath, nearestIndex, niceTicks, rangeWindow, scale, timeTicks, totals, type ChartPoint } from "../src/lib/chart.js";

const point = (t: number, calls: number, errors: number, p50Us: number, p95Us: number): TimePoint => ({
  t,
  calls,
  errors,
  userErrors: errors,
  serverErrors: 0,
  p50Us,
  p95Us,
  maxUs: p95Us,
});

describe("fillBuckets", () => {
  it("fills every bucket of the range; empty ones have no latency", () => {
    const filled = fillBuckets({
      path: "todos:add",
      bucket: 60_000,
      since: 0,
      until: 300_000,
      points: [point(60_000, 4, 1, 1500, 9000), point(240_000, 2, 0, 250, 400)],
    });
    expect(filled.map((p) => p.t)).toEqual([0, 60_000, 120_000, 180_000, 240_000]);
    expect(filled[0]).toEqual({ t: 0, calls: 0, errors: 0, p50: null, p95: null });
    expect(filled[1]).toEqual({ t: 60_000, calls: 4, errors: 1, p50: 1.5, p95: 9 });
    expect(filled[4]).toEqual({ t: 240_000, calls: 2, errors: 0, p50: 0.25, p95: 0.4 });
  });

  it("ignores points outside the range and bad ranges, and caps the bucket count", () => {
    const series = { path: null, bucket: 1000, since: 0, until: 3000, points: [point(5000, 1, 0, 1, 1)] };
    expect(fillBuckets(series).every((p) => p.calls === 0)).toBe(true);
    expect(fillBuckets({ ...series, until: 0 })).toEqual([]);
    expect(fillBuckets({ ...series, bucket: 0 })).toEqual([]);
    expect(fillBuckets({ ...series, until: 1_000_000 }, 10)).toHaveLength(10);
  });
});

describe("totals", () => {
  it("sums calls and errors and finds the worst p95", () => {
    const pts: ChartPoint[] = [
      { t: 0, calls: 10, errors: 1, p50: 1, p95: 4 },
      { t: 1, calls: 0, errors: 0, p50: null, p95: null },
      { t: 2, calls: 30, errors: 3, p50: 2, p95: 12 },
    ];
    expect(totals(pts)).toEqual({ calls: 40, errors: 4, errorRate: 0.1, worstP95: 12 });
    expect(totals([])).toEqual({ calls: 0, errors: 0, errorRate: 0, worstP95: null });
  });
});

describe("niceTicks", () => {
  it("picks round steps that cover the maximum", () => {
    expect(niceTicks(10, 4)).toEqual([0, 2.5, 5, 7.5, 10]);
    expect(niceTicks(7, 4)).toEqual([0, 2, 4, 6, 8]);
    expect(niceTicks(0.37, 4)).toEqual([0, 0.1, 0.2, 0.3, 0.4]);
    expect(niceTicks(950, 4)).toEqual([0, 250, 500, 750, 1000]);
    expect(niceTicks(0)).toEqual([0, 1]);
    for (const max of [1, 3, 17, 123.4, 99_999]) {
      const ticks = niceTicks(max, 4);
      expect(ticks[0]).toBe(0);
      expect(ticks.at(-1)!).toBeGreaterThanOrEqual(max);
      expect(ticks.length).toBeLessThanOrEqual(7);
    }
  });

  it("keeps whole steps for counts", () => {
    expect(niceTicks(1, 4, true)).toEqual([0, 1]);
    expect(niceTicks(2, 4, true)).toEqual([0, 1, 2]);
    expect(niceTicks(10, 4, true)).toEqual([0, 5, 10]);
    expect(niceTicks(7, 4, true)).toEqual([0, 2, 4, 6, 8]);
    for (const max of [0.3, 1, 3, 17, 123.4, 99_999]) {
      const ticks = niceTicks(max, 4, true);
      expect(ticks.every(Number.isInteger)).toBe(true);
      expect(ticks.at(-1)!).toBeGreaterThanOrEqual(max);
    }
  });
});

describe("paths and hit testing", () => {
  const pts: ChartPoint[] = [
    { t: 0, calls: 1, errors: 0, p50: 1, p95: 2 },
    { t: 10, calls: 1, errors: 0, p50: 3, p95: 4 },
    { t: 20, calls: 0, errors: 0, p50: null, p95: null },
    { t: 30, calls: 1, errors: 0, p50: 2, p95: 2 },
  ];
  const x = scale(0, 40, 0, 400);
  const y = scale(0, 4, 100, 0);

  it("breaks lines where there is no data", () => {
    expect(linePath(pts, (p) => p.p50, (t) => x(t + 5), y)).toBe("M50.0,75.0L150.0,25.0M350.0,50.0");
    expect(linePath([], (p) => p.p50, x, y)).toBe("");
  });

  it("finds the bucket under the pointer", () => {
    expect(nearestIndex(pts, 0, x, 10)).toBe(0);
    expect(nearestIndex(pts, 160, x, 10)).toBe(1);
    expect(nearestIndex(pts, 399, x, 10)).toBe(3);
    expect(nearestIndex([], 10, x, 10)).toBe(-1);
  });
});

describe("rangeWindow", () => {
  it("ends with the current bucket and spans whole buckets", () => {
    const now = 10 * 60_000 + 12_345;
    expect(rangeWindow(5 * 60_000, 60_000, now)).toEqual({ since: 6 * 60_000, until: 11 * 60_000 });
  });
});

describe("timeTicks", () => {
  it("picks round times at the smallest step that fits", () => {
    const t0 = Date.UTC(2026, 0, 1, 12, 0, 3);
    const ticks = timeTicks(t0, t0 + 60_000, 4);
    expect(ticks.length).toBeLessThanOrEqual(4);
    expect(ticks.length).toBeGreaterThan(1);
    expect(ticks[1]! - ticks[0]!).toBe(15_000);
    expect(ticks.every((t) => t % 15_000 === 0 && t >= t0 && t <= t0 + 60_000)).toBe(true);
    expect(timeTicks(t0, t0, 4)).toEqual([]);
  });
});
