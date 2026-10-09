/**
 * Canonical JSON: `JSON.stringify` with object keys sorted recursively. Two arg objects that are structurally equal
 * produce the same string, regardless of key insertion order. `undefined` object values are dropped, as in JSON.
 */
export function canonicalJSON(value: unknown): string {
  return JSON.stringify(sortKeys(value)) ?? "null";
}

function sortKeys(value: unknown): unknown {
  if (value === null || typeof value !== "object") return value;
  if (typeof (value as { toJSON?: unknown }).toJSON === "function") {
    return sortKeys((value as { toJSON: () => unknown }).toJSON());
  }
  if (Array.isArray(value)) return value.map(sortKeys);
  const out: Record<string, unknown> = {};
  for (const key of Object.keys(value).sort()) {
    const v = (value as Record<string, unknown>)[key];
    if (v === undefined || typeof v === "function" || typeof v === "symbol") continue;
    out[key] = sortKeys(v);
  }
  return out;
}

/** The client-side identity of a query: `udfPath + canonicalJSON(args)`. */
export type QueryToken = string;

export function queryToken(udfPath: string, args: Record<string, unknown>): QueryToken {
  return udfPath + canonicalJSON(args);
}

/** Structural equality for JSON values (as produced by `JSON.parse`). */
export function jsonEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b || a === null || b === null || typeof a !== "object") {
    // NaN never appears in JSON; plain === suffices for primitives.
    return false;
  }
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!jsonEqual(a[i], b[i])) return false;
    return true;
  }
  if (Array.isArray(b)) return false;
  const ao = a as Record<string, unknown>;
  const bo = b as Record<string, unknown>;
  const aKeys = Object.keys(ao);
  if (aKeys.length !== Object.keys(bo).length) return false;
  for (const k of aKeys) {
    if (!Object.prototype.hasOwnProperty.call(bo, k) || !jsonEqual(ao[k], bo[k])) return false;
  }
  return true;
}

/** Normalizes user-supplied args: `undefined` becomes `{}`; anything else must be a plain object. */
export function normalizeArgs(args: unknown): Record<string, unknown> {
  if (args === undefined || args === null) return {};
  if (typeof args !== "object" || Array.isArray(args)) {
    throw new TypeError(`Ferrix function args must be an object, got ${Array.isArray(args) ? "array" : typeof args}`);
  }
  return args as Record<string, unknown>;
}
