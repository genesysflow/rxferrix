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

/** A span of `ms` milliseconds at a readable scale: `850ms`, `12.4s`, `3m 05s`, `2h 10m`, `3d 4h`. */
export function formatSpan(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const s = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  if (s < 3600) return `${Math.floor(s / 60)}m ${pad(s % 60)}s`;
  if (s < 86_400) return `${Math.floor(s / 3600)}h ${pad(Math.floor((s % 3600) / 60))}m`;
  return `${Math.floor(s / 86_400)}d ${Math.floor((s % 86_400) / 3600)}h`;
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

/** How the Files page previews a file of this content type, or null for no preview. */
export function previewKind(contentType: string | null | undefined): "image" | "video" | "audio" | "text" | null {
  const essence = (contentType ?? "").split(";")[0]!.trim().toLowerCase();
  if (essence.startsWith("image/")) return "image";
  if (essence.startsWith("video/")) return "video";
  if (essence.startsWith("audio/")) return "audio";
  if (essence.startsWith("text/") || /^application\/([\w.+-]+\+)?(json|xml)$/.test(essence)) return "text";
  return null;
}

/**
 * The same-origin path of a server URL (absolute or root-relative), e.g. `/api/storage/<id>`. The dashboard is served
 * by the server itself, and its CSP only allows same-origin requests even when the configured public URL differs.
 */
export function sameOriginPath(url: string): string {
  const u = new URL(url, "http://localhost");
  return u.pathname + u.search;
}

/**
 * `url` with query parameters added (or replaced), keeping the ones it has: a signed storage URL already carries
 * `?exp=…&sig=…`, so variants such as `thumb` must be added with `&`, never a second `?`. Keeps relative URLs
 * relative and the fragment in place.
 */
export function withParams(url: string, params: Record<string, string | number | null | undefined>): string {
  const hashAt = url.indexOf("#");
  const hash = hashAt === -1 ? "" : url.slice(hashAt);
  const base = hashAt === -1 ? url : url.slice(0, hashAt);
  const queryAt = base.indexOf("?");
  const path = queryAt === -1 ? base : base.slice(0, queryAt);
  const query = new URLSearchParams(queryAt === -1 ? "" : base.slice(queryAt + 1));
  for (const [k, v] of Object.entries(params)) {
    if (v === null || v === undefined) query.delete(k);
    else query.set(k, String(v));
  }
  const qs = query.toString();
  return `${path}${qs ? `?${qs}` : ""}${hash}`;
}

/** A server-side thumbnail of a stored image (`thumb=WxH&fit=…`, docs/PROTOCOL.md), on this origin. */
export function thumbUrl(url: string, width: number, height = width, fit: "cover" | "contain" = "cover"): string {
  return sameOriginPath(withParams(url, { thumb: `${width}x${height}`, fit }));
}

/** Thumbnails are generated for these types only (the server answers 415 for others). */
export function hasThumbnail(contentType: string | null | undefined): boolean {
  const essence = (contentType ?? "").split(";")[0]!.trim().toLowerCase();
  return ["image/jpeg", "image/png", "image/webp", "image/gif"].includes(essence);
}

/** A duration in seconds, compactly: `90` → `1m 30s`, `86400` → `1d`. */
export function formatSeconds(total: number): string {
  if (!Number.isFinite(total) || total < 0) return "—";
  const parts: string[] = [];
  let s = Math.round(total);
  for (const [unit, size] of [
    ["d", 86400],
    ["h", 3600],
    ["m", 60],
  ] as const) {
    if (s >= size) {
      parts.push(`${Math.floor(s / size)}${unit}`);
      s %= size;
    }
  }
  if (s > 0 || parts.length === 0) parts.push(`${s}s`);
  return parts.slice(0, 2).join(" ");
}

/** Whether any value of `doc` (keys excluded, nested values included) contains `needle` (lowercase). */
export function documentMatches(doc: Record<string, unknown>, needle: string): boolean {
  if (!needle) return true;
  const visit = (v: unknown): boolean => {
    if (v === null || v === undefined) return false;
    if (typeof v === "string") return v.toLowerCase().includes(needle);
    if (typeof v === "number" || typeof v === "boolean") return String(v).toLowerCase().includes(needle);
    if (Array.isArray(v)) return v.some(visit);
    if (typeof v === "object") return Object.values(v as Record<string, unknown>).some(visit);
    return false;
  };
  return Object.values(doc).some(visit);
}
