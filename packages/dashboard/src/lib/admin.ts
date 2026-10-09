/**
 * Admin-only HTTP endpoints (`/api/admin/*`): the execution log and stats live in `aux.db`, outside the reactive
 * database, so they are read over HTTP with the admin's bearer token instead of through the sync protocol; so are
 * backups, export and import, and impersonation (docs/CONTRACT.md §4.2).
 */
import { FerrixError, type FerrixAuth } from "@ferrix/client";
import type {
  BackupInfo,
  BackupListing,
  ExportFormat,
  Impersonation,
  ImportMode,
  ImportReport,
  LogEntry,
  LogFilter,
  Stats,
  Timeseries,
} from "./api.js";
import { readSse } from "./sse.js";

export function filterQuery(filter: LogFilter): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(filter)) {
    if (v === undefined || v === null || v === "" || v === false) continue;
    p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : "";
}

/** `fetch` with `Authorization: Bearer <admin access token>`, retrying once with a refreshed token on 401. */
export async function adminFetch(auth: FerrixAuth, path: string, init: RequestInit = {}): Promise<Response> {
  for (let attempt = 0; ; attempt++) {
    const token = await auth.getAccessToken({ forceRefresh: attempt > 0 });
    if (!token) throw new FerrixError("Not signed in", { code: "unauthenticated" });
    const headers = new Headers(init.headers);
    headers.set("Authorization", `Bearer ${token}`);
    const res = await fetch(path, { ...init, headers });
    if (res.status === 401 && attempt === 0) continue;
    if (!res.ok) {
      const body = (await res.json().catch(() => null)) as { errorMessage?: string; errorData?: { code: string } } | null;
      throw new FerrixError(body?.errorMessage ?? `HTTP ${res.status}`, body?.errorData ?? { code: "http_error" });
    }
    return res;
  }
}

export async function fetchLogs(auth: FerrixAuth, filter: LogFilter): Promise<LogEntry[]> {
  const res = await adminFetch(auth, `/api/admin/logs${filterQuery(filter)}`);
  return ((await res.json()) as { entries: LogEntry[] }).entries;
}

export async function fetchStats(auth: FerrixAuth): Promise<Stats> {
  const res = await adminFetch(auth, "/api/admin/stats");
  return (await res.json()) as Stats;
}

export interface TailHandlers {
  /** The stream is connected (after every (re)connect). */
  onReady(): void;
  onEntry(entry: LogEntry): void;
  /** The server skipped `n` entries because this client fell behind. */
  onLagged(n: number): void;
  onStatus(status: "connecting" | "live" | "error", error?: string): void;
}

/**
 * Follow the live tail until `signal` aborts. Reconnects (with a fresh token) when the stream ends, which it does
 * when the access token expires, or fails.
 */
export async function tailLogs(auth: FerrixAuth, filter: LogFilter, handlers: TailHandlers, signal: AbortSignal) {
  let delay = 500;
  while (!signal.aborted) {
    handlers.onStatus("connecting");
    try {
      const res = await adminFetch(auth, `/api/admin/logs/stream${filterQuery(filter)}`, { signal });
      for await (const e of readSse(res, signal)) {
        if (e.event === "ready") {
          delay = 500;
          handlers.onStatus("live");
          handlers.onReady();
        } else if (e.event === "log") handlers.onEntry(JSON.parse(e.data) as LogEntry);
        else if (e.event === "lagged") handlers.onLagged(Number(e.data));
      }
    } catch (e) {
      if (signal.aborted) return;
      handlers.onStatus("error", e instanceof Error ? e.message : String(e));
      delay = Math.min(delay * 2, 10_000);
    }
    if (signal.aborted) return;
    await new Promise((r) => setTimeout(r, delay));
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Per-function time series (the Functions page's charts)

export interface TimeseriesOptions {
  /** One function, or every app function when omitted. */
  path?: string | null;
  /** Bucket size in ms. */
  bucket: number;
  since: number;
  until?: number;
}

export async function fetchTimeseries(auth: FerrixAuth, opts: TimeseriesOptions): Promise<Timeseries> {
  const p = new URLSearchParams({ bucket: String(opts.bucket), since: String(Math.floor(opts.since)) });
  if (opts.path) p.set("path", opts.path);
  if (opts.until !== undefined) p.set("until", String(Math.ceil(opts.until)));
  const res = await adminFetch(auth, `/api/admin/stats/timeseries?${p}`);
  return (await res.json()) as Timeseries;
}

// ---------------------------------------------------------------------------------------------------------------------
// Downloads: fetched with the bearer token (never a token in a URL), then saved from a blob.

export interface Download {
  blob: Blob;
  filename: string;
}

/** The file name of a `Content-Disposition` header (`filename*=UTF-8''…` preferred), else `fallback`. */
export function filenameFromDisposition(header: string | null, fallback: string): string {
  if (!header) return fallback;
  const star = /filename\*\s*=\s*UTF-8''([^;]+)/i.exec(header);
  if (star) {
    try {
      return decodeURIComponent(star[1]!.trim());
    } catch {
      // fall through to the plain parameter
    }
  }
  const plain = /filename\s*=\s*(?:"((?:[^"\\]|\\.)*)"|([^;\s]+))/i.exec(header);
  const name = plain?.[1]?.replace(/\\(.)/g, "$1") ?? plain?.[2];
  // Never let a server-provided name carry a path.
  return name ? name.replace(/[/\\]/g, "_") : fallback;
}

async function download(auth: FerrixAuth, path: string, fallback: string): Promise<Download> {
  const res = await adminFetch(auth, path);
  const blob = await res.blob();
  return { blob, filename: filenameFromDisposition(res.headers.get("content-disposition"), fallback) };
}

/** Hand a downloaded blob to the browser as a file to save. */
export function saveBlob({ blob, filename }: Download, doc: Document = document): void {
  const url = URL.createObjectURL(blob);
  const a = doc.createElement("a");
  a.href = url;
  a.download = filename;
  a.rel = "noopener";
  a.style.display = "none";
  doc.body.appendChild(a);
  a.click();
  a.remove();
  // Revoking right away can cancel the download in some browsers.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

/** `GET /api/admin/export/{table}?format=…`: the whole table from one snapshot (secrets of system tables masked). */
export function exportTable(auth: FerrixAuth, table: string, format: ExportFormat): Promise<Download> {
  return download(auth, `/api/admin/export/${encodeURIComponent(table)}?format=${format}`, `${table}.${format}`);
}

/** `POST /api/admin/import/{table}?mode=…&format=…` with a JSONL or CSV body (CSV as exported, with a header). */
export async function importTable(
  auth: FerrixAuth,
  table: string,
  mode: ImportMode,
  body: Blob | string,
  format: ExportFormat = "jsonl",
): Promise<ImportReport> {
  const res = await adminFetch(auth, `/api/admin/import/${encodeURIComponent(table)}?mode=${mode}&format=${format}`, {
    method: "POST",
    headers: { "Content-Type": format === "csv" ? "text/csv" : "application/x-ndjson" },
    body,
  });
  return (await res.json()) as ImportReport;
}

/** The import format a file name suggests: `.csv` is CSV, anything else JSONL. */
export function importFormatOf(name: string): ExportFormat {
  return /\.csv$/i.test(name) ? "csv" : "jsonl";
}

// ---------------------------------------------------------------------------------------------------------------------
// Backups

export async function listBackups(auth: FerrixAuth): Promise<BackupListing> {
  const res = await adminFetch(auth, "/api/admin/backups");
  const r = (await res.json()) as Partial<BackupListing> & { target: string; backups: BackupInfo[] };
  return { pendingRestore: null, restart: { available: false, exitCode: null }, ...r };
}

/** The pending restore and restart availability, as the staging endpoints answer. */
export type RestoreState = Pick<BackupListing, "pendingRestore" | "restart">;

/**
 * `POST /api/admin/backups/{name}/restore`: validate a stored backup completely (checksums, SQLite integrity, schema,
 * indexes) and stage it to replace the database at the next start. Throws with the reason when it can't be restored.
 */
export async function stageRestore(auth: FerrixAuth, name: string): Promise<RestoreState> {
  const res = await adminFetch(auth, `/api/admin/backups/${encodeURIComponent(name)}/restore`, { method: "POST" });
  return (await res.json()) as RestoreState;
}

/** `POST /api/admin/restore?name=…` with an archive from this computer: the same validation and staging. */
export async function uploadRestore(auth: FerrixAuth, file: Blob, name: string): Promise<RestoreState> {
  const res = await adminFetch(auth, `/api/admin/restore?name=${encodeURIComponent(name)}`, {
    method: "POST",
    headers: { "Content-Type": "application/zstd" },
    body: file,
  });
  return (await res.json()) as RestoreState;
}

/** `DELETE /api/admin/restore`: cancel the staged restore. */
export async function cancelRestore(auth: FerrixAuth): Promise<void> {
  await adminFetch(auth, "/api/admin/restore", { method: "DELETE" });
}

/** `POST /api/admin/restore/restart`: the server shuts down gracefully and exits for its supervisor to restart it. */
export async function restartServer(auth: FerrixAuth): Promise<{ restarting: boolean; exitCode: number }> {
  const res = await adminFetch(auth, "/api/admin/restore/restart", { method: "POST" });
  return (await res.json()) as { restarting: boolean; exitCode: number };
}

/** Resolves once `GET /api/health` answers again after it stopped answering (or after `timeoutMs`: `false`). */
export async function waitForRestart(timeoutMs = 120_000, interval = 500): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  let wentDown = false;
  while (Date.now() < deadline) {
    try {
      const ok = (await fetch("/api/health", { cache: "no-store" })).ok;
      if (ok && wentDown) return true;
      if (!ok) wentDown = true;
    } catch {
      wentDown = true;
    }
    await new Promise((r) => setTimeout(r, interval));
  }
  return false;
}

export async function createBackup(
  auth: FerrixAuth,
  options: { includeAux: boolean; includeFiles: boolean },
): Promise<BackupInfo> {
  const res = await adminFetch(auth, "/api/admin/backups", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(options),
  });
  return (await res.json()) as BackupInfo;
}

export function downloadBackup(auth: FerrixAuth, name: string): Promise<Download> {
  return download(auth, `/api/admin/backups/${encodeURIComponent(name)}`, name);
}

export async function deleteBackup(auth: FerrixAuth, name: string): Promise<void> {
  await adminFetch(auth, `/api/admin/backups/${encodeURIComponent(name)}`, { method: "DELETE" });
}

// ---------------------------------------------------------------------------------------------------------------------
// Impersonation

/**
 * `POST /api/admin/impersonate`: a short-lived session of `user`. The tokens are dropped here: only the app URL
 * (which carries them in its fragment, to be opened in a new tab) and the expiry are kept.
 */
export async function impersonate(
  auth: FerrixAuth,
  body: { user: string; ttlSeconds: number; redirectTo?: string | null },
): Promise<Impersonation> {
  const res = await adminFetch(auth, "/api/admin/impersonate", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const r = (await res.json()) as Impersonation & { accessToken?: string; refreshToken?: string };
  return { userId: r.userId, sessionId: r.sessionId, expiresAt: r.expiresAt, url: r.url ?? null };
}
