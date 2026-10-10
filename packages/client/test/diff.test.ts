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
