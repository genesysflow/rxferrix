/**
 * What changed between two snapshots of the live document grid (unit-tested in test/changes.test.ts), so the grid can
 * animate it: documents that appeared, fields that changed, documents that went away (shown as fading "ghost" rows
 * for a moment, next to the row they followed).
 */

import type { JsonObject } from "./api.js";

/** One snapshot: the documents in order, each with its serialized form (to compare cheaply). */
export interface Snapshot {
  ids: string[];
  docs: Map<string, { doc: JsonObject; json: string }>;
  /** More could be loaded after the last document (so documents appearing at the end are a page, not inserts). */
  canLoadMore: boolean;
}

export interface Removed {
  id: string;
  doc: JsonObject;
  /** The id of the document it came after (null: it was first). */
  after: string | null;
}

export interface Changes {
  inserted: string[];
  /** Changed documents and the fields that differ (added, removed or different values). */
  changed: Map<string, string[]>;
  removed: Removed[];
}

export function snapshot(docs: JsonObject[], canLoadMore: boolean): Snapshot {
  const map = new Map<string, { doc: JsonObject; json: string }>();
  for (const doc of docs) map.set(doc._id as string, { doc, json: JSON.stringify(doc) });
  return { ids: docs.map((d) => d._id as string), docs: map, canLoadMore };
}

/**
 * The changes from `prev` to `next`. A document that is new to `next` counts as inserted only if it is within the
 * range `prev` covered: before its last document, or anywhere if `prev` had everything (no more pages). Documents
 * past the end of `prev` are a page that loaded.
 */
export function diffSnapshots(prev: Snapshot, next: Snapshot): Changes {
  const inserted: string[] = [];
  const changed = new Map<string, string[]>();
  const removed: Removed[] = [];
  // Where `prev`'s range ends in `next`: the position of the last document of `prev` that is still there.
  let covered = -1;
  for (let i = prev.ids.length - 1; i >= 0 && covered < 0; i--) {
    const at = next.ids.indexOf(prev.ids[i]!);
    if (at >= 0) covered = at;
  }
  next.ids.forEach((id, i) => {
    const before = prev.docs.get(id);
    if (!before) {
      if (!prev.canLoadMore || i < covered) inserted.push(id);
      return;
    }
    const after = next.docs.get(id)!;
    if (before.json !== after.json) changed.set(id, changedFields(before.doc, after.doc));
  });
  prev.ids.forEach((id, i) => {
    if (next.docs.has(id)) return;
    removed.push({ id, doc: prev.docs.get(id)!.doc, after: i === 0 ? null : prev.ids[i - 1]! });
  });
  return { inserted, changed, removed };
}

function changedFields(a: JsonObject, b: JsonObject): string[] {
  const fields = new Set([...Object.keys(a), ...Object.keys(b)]);
  return [...fields].filter((f) => JSON.stringify(a[f]) !== JSON.stringify(b[f]));
}

/**
 * `rows` with the removed documents put back right after the document they followed (or first), for as long as they
 * fade out. A ghost whose anchor is not shown (filtered out, or itself gone and not a ghost) is left out.
 */
export function withGhosts(rows: JsonObject[], ghosts: Removed[]): { doc: JsonObject; ghost: boolean }[] {
  const out = rows.map((doc) => ({ doc, ghost: false }));
  if (ghosts.length === 0) return out;
  const byAnchor = new Map<string | null, Removed[]>();
  for (const g of ghosts) byAnchor.set(g.after, [...(byAnchor.get(g.after) ?? []), g]);
  const result: { doc: JsonObject; ghost: boolean }[] = [];
  const place = (anchor: string | null) => {
    for (const g of byAnchor.get(anchor) ?? []) {
      result.push({ doc: g.doc, ghost: true });
      place(g.id); // a ghost that followed this ghost
    }
  };
  place(null);
  for (const row of out) {
    result.push(row);
    place(row.doc._id as string);
  }
  return result;
}
