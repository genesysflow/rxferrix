/**
 * Result diffs (docs/PROTOCOL.md, "Result diffs"): the server sends what changed in a query result instead of the
 * whole result, relative to the result this client already has. The format, applied to the previous result:
 *
 * - `{"$r": value}`: replace with `value`.
 * - `{"$a": [op, …]}`: edit an array. The ops walk the old array from the start: a positive integer `n` keeps the next
 *   `n` elements, a negative integer `-n` deletes them, an array inserts its elements, and `{"$p": diff}` patches the
 *   next element. Elements after the last op are kept.
 * - `{"$o": {"s": {…}, "d": […], "p": {…}}}`: edit an object: set, delete and patch keys.
 *
 * Unchanged elements and objects are reused, not copied, so React and Svelte can skip re-rendering them.
 */

/** A diff didn't fit the result it was applied to: the client's copy is not what the server diffed against. */
export class DiffMismatch extends Error {
  constructor(message: string) {
    super(`result diff doesn't apply: ${message}`);
  }
}

type Json = unknown;

function isObject(v: Json): v is Record<string, Json> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Sets `key` as a plain own property: `out[key] = value` would set the prototype for `"__proto__"` (a key data can
 * have, e.g. in a map keyed by user input). */
function setOwn(out: Record<string, Json>, key: string, value: Json): void {
  Object.defineProperty(out, key, { value, enumerable: true, writable: true, configurable: true });
}

export function applyDiff(old: Json, diff: Json): Json {
  if (!isObject(diff)) throw new DiffMismatch("a diff must be an object");
  if ("$r" in diff) return diff.$r;
  if ("$a" in diff) {
    const ops = diff.$a;
    if (!Array.isArray(old) || !Array.isArray(ops)) throw new DiffMismatch("array diff applied to a non-array");
    const out: Json[] = [];
    let i = 0;
    for (const op of ops) {
      if (typeof op === "number") {
        const count = Math.abs(op);
        if (op === 0 || !Number.isInteger(op) || i + count > old.length) {
          throw new DiffMismatch(`op ${op} at ${i} runs past ${old.length} elements`);
        }
        if (op > 0) for (let k = i; k < i + count; k++) out.push(old[k]);
        i += count;
      } else if (Array.isArray(op)) {
        for (const item of op) out.push(item);
      } else if (isObject(op) && "$p" in op) {
        if (i >= old.length) throw new DiffMismatch("patch past the end");
        out.push(applyDiff(old[i], op.$p));
        i += 1;
      } else {
        throw new DiffMismatch(`bad array op ${JSON.stringify(op)}`);
      }
    }
    for (let k = i; k < old.length; k++) out.push(old[k]);
    return out;
  }
  if ("$o" in diff) {
    const o = diff.$o;
    if (!isObject(old) || !isObject(o)) throw new DiffMismatch("object diff applied to a non-object");
    const out: Record<string, Json> = { ...old };
    if (o.d !== undefined) {
      if (!Array.isArray(o.d)) throw new DiffMismatch("bad deleted keys");
      for (const key of o.d) {
        if (typeof key !== "string" || !Object.hasOwn(out, key)) throw new DiffMismatch(`deleting missing key ${String(key)}`);
        delete out[key];
      }
    }
    if (o.s !== undefined) {
      if (!isObject(o.s)) throw new DiffMismatch("bad set keys");
      for (const [key, value] of Object.entries(o.s)) setOwn(out, key, value);
    }
    if (o.p !== undefined) {
      if (!isObject(o.p)) throw new DiffMismatch("bad patched keys");
      for (const [key, sub] of Object.entries(o.p)) {
        if (!Object.hasOwn(old, key)) throw new DiffMismatch(`patching missing key ${key}`);
        setOwn(out, key, applyDiff(old[key], sub));
      }
    }
    return out;
  }
  throw new DiffMismatch(`unknown diff ${JSON.stringify(diff)}`);
}
