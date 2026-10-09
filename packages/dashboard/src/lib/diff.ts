/**
 * A field-level diff of an audit entry's `before` / `after` (unit-tested in test/diff.test.ts). Inserts have only
 * `after`, deletes only `before`, updates the changed fields on both sides.
 */
import type { Json } from "./api.js";

export type ChangeKind = "added" | "removed" | "changed" | "same";

export interface FieldChange {
  field: string;
  kind: ChangeKind;
  before: Json | undefined;
  after: Json | undefined;
}

function isObject(v: Json | null | undefined): v is { [key: string]: Json } {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function equal(a: Json | undefined, b: Json | undefined): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * One row per top-level field of either side (`_id` first, system fields last, the rest sorted). Non-object sides
 * are compared as a whole under the field name `(value)`.
 */
export function diffFields(before: Json | null | undefined, after: Json | null | undefined): FieldChange[] {
  if ((before != null && !isObject(before)) || (after != null && !isObject(after))) {
    if (equal(before ?? undefined, after ?? undefined)) return [];
    return [{ field: "(value)", kind: kindOf(before ?? undefined, after ?? undefined), before: before ?? undefined, after: after ?? undefined }];
  }
  const b = isObject(before) ? before : {};
  const a = isObject(after) ? after : {};
  const fields = [...new Set([...Object.keys(b), ...Object.keys(a)])].sort((x, y) => rank(x) - rank(y) || (x < y ? -1 : x > y ? 1 : 0));
  return fields.map((field) => ({ field, kind: kindOf(b[field], a[field]), before: b[field], after: a[field] }));
}

function kindOf(before: Json | undefined, after: Json | undefined): ChangeKind {
  if (before === undefined) return "added";
  if (after === undefined) return "removed";
  return equal(before, after) ? "same" : "changed";
}

function rank(field: string): number {
  return field === "_id" ? 0 : field.startsWith("_") ? 2 : 1;
}

/** A one-line summary: `3 changed, 1 added`. */
export function summarize(changes: readonly FieldChange[]): string {
  const counts: Partial<Record<ChangeKind, number>> = {};
  for (const c of changes) counts[c.kind] = (counts[c.kind] ?? 0) + 1;
  const parts = (["changed", "added", "removed"] as const).filter((k) => counts[k]).map((k) => `${counts[k]} ${k}`);
  return parts.join(", ") || "no field changes";
}
