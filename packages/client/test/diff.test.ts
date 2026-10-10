import { describe, expect, it } from "vitest";
import { applyDiff, DiffMismatch } from "../src/index.js";
import { RemoteQuerySet } from "../src/remote_query_set.js";

import fixtures from "./fixtures/diffs.json";

/** (old, new, diff) cases from the server's diff algorithm (crates/ferrix/tests/it/diff_fixtures.rs). */
const cases = fixtures as { old: unknown; new: unknown; diff: unknown }[];

describe("applyDiff", () => {
  it("turns the old result into the new one for every server-generated case", () => {
    expect(cases.length).toBeGreaterThanOrEqual(300);
    for (const { old, new: expected, diff } of cases) {
      expect(applyDiff(old, diff)).toEqual(expected);
    }
  });

  it("does not modify the old result, and reuses what didn't change", () => {
    const keep = { _id: "a", tags: ["x"] };
    const old = { page: [keep, { _id: "b", n: 1 }], isDone: false };
    const snapshot = structuredClone(old);
    const next = applyDiff(old, { $o: { s: { isDone: true }, p: { page: { $a: [1, { $p: { $o: { s: { n: 2 } } } }] } } } }) as typeof old;
    expect(old).toEqual(snapshot);
    expect(next).toEqual({ page: [keep, { _id: "b", n: 2 }], isDone: true });
    expect(next.page[0]).toBe(keep);
  });

  it("rejects diffs that don't fit", () => {
    const bad: [unknown, unknown][] = [
      [[1, 2], { $a: [3] }],
      [[1, 2], { $a: [-3] }],
      [[1], { $a: [0] }],
      [{ a: 1 }, { $o: { d: ["b"] } }],
      [{ a: 1 }, { $o: { p: { b: { $r: 1 } } } }],
      [1, { $a: [] }],
      [[], { $p: 1 }],
      [[], "nope"],
    ];
    for (const [old, diff] of bad) expect(() => applyDiff(old, diff)).toThrow(DiffMismatch);
  });
});

describe("RemoteQuerySet with diffs", () => {
  const known = () => true;

  it("applies a transition's diffs atomically", () => {
    const set = new RemoteQuerySet();
    set.transition(
      {
        type: "Transition",
        startVersion: { querySet: 0, identity: 0, ts: 0 },
        endVersion: { querySet: 1, identity: 0, ts: 1 },
        modifications: [
          { type: "QueryUpdated", queryId: 1, value: [1, 2] },
          { type: "QueryUpdated", queryId: 2, value: { a: 1 } },
        ],
      },
      known,
    );
    // The second diff doesn't fit: the first must not be applied either, nor the version advanced.
    expect(() =>
      set.transition(
        {
          type: "Transition",
          startVersion: { querySet: 1, identity: 0, ts: 1 },
          endVersion: { querySet: 1, identity: 0, ts: 2 },
          modifications: [
            { type: "QueryUpdated", queryId: 1, diff: { $a: [[0]] } },
            { type: "QueryUpdated", queryId: 2, diff: { $a: [] } },
          ],
        },
        known,
      ),
    ).toThrow(DiffMismatch);
    expect(set.result(1)).toEqual({ status: "success", value: [1, 2] });
    expect(set.version.ts).toBe(1);
  });
});

describe("applyDiff on garbage", () => {
  // A small seeded PRNG (mulberry32), so failures are reproducible.
  function prng(seed: number) {
    return () => {
      seed |= 0;
      seed = (seed + 0x6d2b79f5) | 0;
      let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  it("only ever throws DiffMismatch, whatever it is given", () => {
    const rand = prng(42);
    const int = (n: number) => Math.floor(rand() * n);
    const keys = ["a", "b", "__proto__", "toString", "constructor", "$a", "$o", "$p", "$r", "s", "d", "p"];
    const value = (depth: number): unknown => {
      switch (int(depth > 0 ? 9 : 6)) {
        case 0: return null;
        case 1: return rand() < 0.5;
        case 2: return [0, -1, 1, 2.5, -3, 1e308, -1e308, Number.MAX_SAFE_INTEGER, NaN][int(9)];
        case 3: return ["", "x", "__proto__"][int(3)];
        case 4: return keys[int(keys.length)];
        case 5: return undefined;
        case 6: return Array.from({ length: int(5) }, () => value(depth - 1));
        default: {
          const o: Record<string, unknown> = {};
          for (let i = int(4); i > 0; i--) Object.defineProperty(o, keys[int(keys.length)]!, { value: value(depth - 1), enumerable: true, writable: true, configurable: true });
          return o;
        }
      }
    };
    const diff = (depth: number): unknown => {
      switch (int(5)) {
        case 0: return { $r: value(2) };
        case 1: return { $a: Array.from({ length: int(5) }, () => (rand() < 0.3 ? { $p: depth > 0 ? diff(depth - 1) : value(1) } : value(1))) };
        case 2: return { $o: { s: value(1), d: value(1), p: depth > 0 ? { [keys[int(keys.length)]!]: diff(depth - 1) } : value(1) } };
        case 3: return value(2);
        default: return { [keys[int(keys.length)]!]: value(2) };
      }
    };
    let applied = 0;
    for (let i = 0; i < 20_000; i++) {
      const old = value(3);
      const d = diff(3);
      try {
        applyDiff(old, d);
        applied++;
      } catch (e) {
        if (!(e instanceof DiffMismatch)) throw new Error(`case ${i}: ${String(e)}\nold ${JSON.stringify(old)}\ndiff ${JSON.stringify(d)}`);
      }
    }
    expect(applied).toBeGreaterThan(1000); // some garbage happens to fit: those paths ran too
    expect(Object.prototype).not.toHaveProperty("x"); // nothing leaked into the global prototype
  });
});
