import { describe, expect, it } from "vitest";
import type { JsonObject } from "../src/lib/api.js";
import { diffSnapshots, snapshot, withGhosts } from "../src/lib/changes.js";

const doc = (id: string, fields: JsonObject = {}): JsonObject => ({ _id: id, _creationTime: 1, ...fields });
const ids = (rows: { doc: JsonObject; ghost: boolean }[]) => rows.map((r) => `${r.doc._id}${r.ghost ? "*" : ""}`);

describe("diffSnapshots", () => {
  it("finds inserted, changed and removed documents", () => {
    const prev = snapshot([doc("a", { n: 1 }), doc("b", { n: 2, s: "x" }), doc("c")], false);
    const next = snapshot([doc("new"), doc("a", { n: 1 }), doc("b", { n: 3, t: true }), doc("d")], false);
    const changes = diffSnapshots(prev, next);
    expect(changes.inserted).toEqual(["new", "d"]);
    expect([...changes.changed]).toEqual([["b", ["n", "s", "t"]]]);
    expect(changes.removed).toEqual([{ id: "c", doc: doc("c"), after: "b" }]);
  });

  it("treats documents past the end of a partial snapshot as a loaded page", () => {
    const prev = snapshot([doc("a"), doc("b")], true);
    const next = snapshot([doc("top"), doc("a"), doc("mid"), doc("b"), doc("p1"), doc("p2")], true);
    expect(diffSnapshots(prev, next).inserted).toEqual(["top", "mid"]);
  });

  it("reports nothing for the same documents", () => {
    const docs = [doc("a", { n: 1 }), doc("b")];
    const changes = diffSnapshots(snapshot(docs, false), snapshot(structuredClone(docs), false));
    expect(changes).toEqual({ inserted: [], changed: new Map(), removed: [] });
  });
});

describe("withGhosts", () => {
  it("puts removed documents back after the row they followed", () => {
    const rows = [doc("a"), doc("c")];
    const ghosts = [
      { id: "first", doc: doc("first"), after: null },
      { id: "b", doc: doc("b"), after: "a" },
      { id: "b2", doc: doc("b2"), after: "b" },
      { id: "orphan", doc: doc("orphan"), after: "filtered-out" },
    ];
    expect(ids(withGhosts(rows, ghosts))).toEqual(["first*", "a", "b*", "b2*", "c"]);
  });
});
