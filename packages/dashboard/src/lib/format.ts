/** Pure formatting helpers (unit-tested in test/format.test.ts). */
import type { Json } from "./api.js";

const ALPHABET = "0123456789abcdefghjkmnpqrstvwxyz";
const ID_RE = /^[0-9a-hjkmnp-tv-z]{26}$/;

/**
 * The table number encoded in a document id (its first 4 bytes, big-endian; ids are 26 chars of lowercase Crockford
 * base32 of 16 bytes, see crates/ferrix/src/id.rs), or null if `value` is not an id.
 */
export function idTableNumber(value: unknown): number | null {
  if (typeof value !== "string" || !ID_RE.test(value)) return null;
  // 16 bytes = 128 bits in 26 chars of 5 bits (130): the last char's 2 low bits are padding and must be zero.
  const last = ALPHABET.indexOf(value[25]!);
  if ((last & 0b11) !== 0) return null;
  let bits = 0;
  for (let i = 0; i < 7; i++) bits = bits * 32 + ALPHABET.indexOf(value[i]!); // 35 bits
  return Math.floor(bits / 8); // the top 32 bits
}

/** Compact one-line JSON for grid cells, cut to `max` characters. */
export function compactJson(value: Json | undefined, max = 80): string {
  if (value === undefined) return "";
  const s = typeof value === "string" ? value : JSON.stringify(value);
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}

/** Column names for a set of documents: `_id` first, then user fields (first-seen order), `_creationTime` last. */
export function columnsOf(docs: readonly Record<string, unknown>[]): string[] {
  const seen = new Set<string>();
  for (const d of docs) for (const k of Object.keys(d)) seen.add(k);
  seen.delete("_id");
  seen.delete("_creationTime");
  return ["_id", ...seen, "_creationTime"];
}

export function formatTime(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "—";
  const d = new Date(ms);
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

export function formatClock(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

export function formatDuration(us: number): string {
  if (us < 1000) return `${Math.round(us)}µs`;
  if (us < 1_000_000) return `${(us / 1000).toFixed(us < 10_000 ? 1 : 0)}ms`;
  return `${(us / 1_000_000).toFixed(2)}s`;
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export function formatAgo(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/** A time relative to `now`: `in 5m`, `12s ago`, `now`. */
export function formatRelative(ms: number, now = Date.now()): string {
  const d = Math.round((ms - now) / 1000);
  if (d === 0) return "now";
  const s = Math.abs(d);
  const amount =
    s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86400 ? `${Math.floor(s / 3600)}h` : `${Math.floor(s / 86400)}d`;
  return d > 0 ? `in ${amount}` : `${amount} ago`;
}

export function formatUptime(ms: number): string {
  const s = Math.floor(ms / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  return d > 0 ? `${d}d ${h}h` : h > 0 ? `${h}h ${m}m` : `${m}m ${s % 60}s`;
}

/** Parse JSON text, returning a readable error instead of throwing. */
export function parseJson(text: string): { ok: true; value: Json } | { ok: false; error: string } {
  try {
    return { ok: true, value: JSON.parse(text) as Json };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/**
 * A filter value typed into an input: JSON if it parses (`42`, `true`, `"x"`, `null`), otherwise the raw text as a
 * string, so plain words need no quotes.
 */
export function parseFilterValue(text: string): Json {
  const t = text.trim();
  const parsed = parseJson(t);
  return parsed.ok ? parsed.value : t;
}

export function pretty(value: unknown): string {
  return JSON.stringify(value, null, 2);
}

/** Pretty-print logged args (which may be truncated and then not valid JSON). */
export function prettyArgs(args: string): string {
  const parsed = parseJson(args);
  return parsed.ok ? pretty(parsed.value) : args;
}
