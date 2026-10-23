/**
 * Admin-only HTTP endpoints (`/api/admin/*`): the execution log and stats live in `aux.db`, outside the reactive
 * database, so they are read over HTTP with the admin's bearer token instead of through the sync protocol; so are
 * backups, export and import, and impersonation (docs/CONTRACT.md §4.2).
 */
import { RxFerrixError, type RxFerrixAuth } from "@rxferrix/client";
import type {
  BackupInfo,
  BackupListing,
  ClientIp,
  ExportFormat,
  Impersonation,
  ImportMode,
  ImportReport,
  Json,
  LogEntry,
  LogFilter,
  OlapResult,
  OlapStatus,
  ReplicationSnapshot,
  ReplicationStatus,
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
export async function adminFetch(auth: RxFerrixAuth, path: string, init: RequestInit = {}): Promise<Response> {
  for (let attempt = 0; ; attempt++) {
    const token = await auth.getAccessToken({ forceRefresh: attempt > 0 });
    if (!token) throw new RxFerrixError("Not signed in", { code: "unauthenticated" });
    const headers = new Headers(init.headers);
    headers.set("Authorization", `Bearer ${token}`);
    const res = await fetch(path, { ...init, headers });
    if (res.status === 401 && attempt === 0) continue;
    if (!res.ok) {
      const body = (await res.json().catch(() => null)) as { errorMessage?: string; errorData?: { code: string } } | null;
      throw new RxFerrixError(body?.errorMessage ?? `HTTP ${res.status}`, body?.errorData ?? { code: "http_error" });
    }
    return res;
  }
}

export async function fetchLogs(auth: RxFerrixAuth, filter: LogFilter): Promise<LogEntry[]> {
  const res = await adminFetch(auth, `/api/admin/logs${filterQuery(filter)}`);
  return ((await res.json()) as { entries: LogEntry[] }).entries;
}

/** `GET /api/admin/stats`; the per-function stats without background work with `excludeBackground`. */
export async function fetchStats(auth: RxFerrixAuth, opts: { excludeBackground?: boolean } = {}): Promise<Stats> {
  const res = await adminFetch(auth, `/api/admin/stats${opts.excludeBackground ? "?excludeBackground=true" : ""}`);
  return (await res.json()) as Stats;
}

/** `GET /api/admin/client-ip`: this request's client IP as the server resolves it, and the proxy headers it carried. */
export async function fetchClientIp(auth: RxFerrixAuth): Promise<ClientIp> {
  const res = await adminFetch(auth, "/api/admin/client-ip");
  return (await res.json()) as ClientIp;
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
export async function tailLogs(auth: RxFerrixAuth, filter: LogFilter, handlers: TailHandlers, signal: AbortSignal) {
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
  /** Leave out scheduled jobs (crons and workflow steps included) and the functions they call. */
  excludeBackground?: boolean;
}

export async function fetchTimeseries(auth: RxFerrixAuth, opts: TimeseriesOptions): Promise<Timeseries> {
  const p = new URLSearchParams({ bucket: String(opts.bucket), since: String(Math.floor(opts.since)) });
  if (opts.path) p.set("path", opts.path);
  if (opts.until !== undefined) p.set("until", String(Math.ceil(opts.until)));
  if (opts.excludeBackground) p.set("excludeBackground", "true");
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

async function download(auth: RxFerrixAuth, path: string, fallback: string): Promise<Download> {
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
export function exportTable(auth: RxFerrixAuth, table: string, format: ExportFormat): Promise<Download> {
  return download(auth, `/api/admin/export/${encodeURIComponent(table)}?format=${format}`, `${table}.${format}`);
}

/** `POST /api/admin/import/{table}?mode=…&format=…` with a JSONL or CSV body (CSV as exported, with a header). */
export async function importTable(
  auth: RxFerrixAuth,
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

export async function listBackups(auth: RxFerrixAuth): Promise<BackupListing> {
  const res = await adminFetch(auth, "/api/admin/backups");
  const r = (await res.json()) as Partial<BackupListing> & { target: string; backups: BackupInfo[] };
  return { pendingRestore: null, restart: { available: false, exitCode: null }, replication: null, ...r };
}

/** `GET /api/admin/replication`: the replication status, or null when replication is off. */
export async function getReplication(auth: RxFerrixAuth): Promise<ReplicationStatus | null> {
  const res = await adminFetch(auth, "/api/admin/replication");
  const r = (await res.json()) as { enabled: boolean } & Partial<ReplicationStatus>;
  if (!r.enabled) return null;
  const { enabled: _, ...status } = r;
  return status as ReplicationStatus;
}

/** `POST /api/admin/replication/snapshot`: take a snapshot of the current generation now. */
export async function replicationSnapshot(auth: RxFerrixAuth): Promise<ReplicationSnapshot> {
  const res = await adminFetch(auth, "/api/admin/replication/snapshot", { method: "POST" });
  return (await res.json()) as ReplicationSnapshot;
}

/** Which point a point-in-time restore restores: a commit ts or a time (ms), in a generation (default: the newest). */
export interface PointInTimeRequest {
  toTs?: number;
  toTime?: number;
  generation?: string;
}

/**
 * `POST /api/admin/restore/point-in-time`: restore the replicated state at a commit ts or time (the newest snapshot
 * before it plus the journal up to it), check it like a backup, and stage it for the next start.
 */
export async function stagePointInTime(auth: RxFerrixAuth, point: PointInTimeRequest): Promise<RestoreState> {
  const res = await adminFetch(auth, "/api/admin/restore/point-in-time", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(point),
  });
  return (await res.json()) as RestoreState;
}

/** The pending restore and restart availability, as the staging endpoints answer. */
export type RestoreState = Pick<BackupListing, "pendingRestore" | "restart">;

/**
 * `POST /api/admin/backups/{name}/restore`: validate a stored backup completely (checksums, SQLite integrity, schema,
 * indexes) and stage it to replace the database at the next start. Throws with the reason when it can't be restored.
 */
export async function stageRestore(auth: RxFerrixAuth, name: string): Promise<RestoreState> {
  const res = await adminFetch(auth, `/api/admin/backups/${encodeURIComponent(name)}/restore`, { method: "POST" });
  return (await res.json()) as RestoreState;
}

/** `POST /api/admin/restore?name=…` with an archive from this computer: the same validation and staging. */
export async function uploadRestore(auth: RxFerrixAuth, file: Blob, name: string): Promise<RestoreState> {
  const res = await adminFetch(auth, `/api/admin/restore?name=${encodeURIComponent(name)}`, {
    method: "POST",
    headers: { "Content-Type": "application/zstd" },
    body: file,
  });
  return (await res.json()) as RestoreState;
}

/** `DELETE /api/admin/restore`: cancel the staged restore. */
export async function cancelRestore(auth: RxFerrixAuth): Promise<void> {
  await adminFetch(auth, "/api/admin/restore", { method: "DELETE" });
}

/** `POST /api/admin/restore/restart`: the server shuts down gracefully and exits for its supervisor to restart it. */
export async function restartServer(auth: RxFerrixAuth): Promise<{ restarting: boolean; exitCode: number }> {
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
  auth: RxFerrixAuth,
  options: { includeAux: boolean; includeFiles: boolean },
): Promise<BackupInfo> {
  const res = await adminFetch(auth, "/api/admin/backups", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(options),
  });
  return (await res.json()) as BackupInfo;
}

export function downloadBackup(auth: RxFerrixAuth, name: string): Promise<Download> {
  return download(auth, `/api/admin/backups/${encodeURIComponent(name)}`, name);
}

export async function deleteBackup(auth: RxFerrixAuth, name: string): Promise<void> {
  await adminFetch(auth, `/api/admin/backups/${encodeURIComponent(name)}`, { method: "DELETE" });
}

// ---------------------------------------------------------------------------------------------------------------------
// Impersonation

/**
 * `POST /api/admin/impersonate`: a short-lived session of `user`. The tokens are dropped here: only the app URL
 * (which carries them in its fragment, to be opened in a new tab) and the expiry are kept.
 */
export async function impersonate(
  auth: RxFerrixAuth,
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

// ---------------------------------------------------------------------------------------------------------------------
// Analytics (`--olap`)

/** `GET /api/admin/olap`: the analytics copy's status, or null when the server runs without `--olap`. */
export async function getOlap(auth: RxFerrixAuth): Promise<OlapStatus | null> {
  const res = await adminFetch(auth, "/api/admin/olap");
  const r = (await res.json()) as { enabled: boolean } & Partial<OlapStatus>;
  if (!r.enabled) return null;
  const { enabled: _, ...status } = r;
  return status as OlapStatus;
}

/** `POST /api/admin/olap/query`: one SELECT over the analytics copy (the sandbox of `ctx.olap`; audited). */
export async function olapQuery(auth: RxFerrixAuth, sql: string, params: Json[] = []): Promise<OlapResult> {
  const res = await adminFetch(auth, "/api/admin/olap/query", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ sql, params }),
  });
  return (await res.json()) as OlapResult;
}

/** The whole result of one SELECT as a CSV or Parquet file. */
export async function olapExport(auth: RxFerrixAuth, sql: string, format: "csv" | "parquet", params: Json[] = []): Promise<Download> {
  const res = await adminFetch(auth, "/api/admin/olap/query", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ sql, params, format }),
  });
  const blob = await res.blob();
  return { blob, filename: filenameFromDisposition(res.headers.get("content-disposition"), `query.${format}`) };
}

/** `POST /api/admin/olap/rebuild`: rebuild every table of the copy from a snapshot, in the background. */
export async function olapRebuild(auth: RxFerrixAuth): Promise<void> {
  await adminFetch(auth, "/api/admin/olap/rebuild", { method: "POST" });
}
