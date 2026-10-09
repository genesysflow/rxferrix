/**
 * Admin-only HTTP endpoints (`/api/admin/*`): the execution log and stats live in `aux.db`, outside the reactive
 * database, so they are read over HTTP with the admin's bearer token instead of through the sync protocol.
 */
import { FerrixError, type FerrixAuth } from "@ferrix/client";
import type { LogEntry, LogFilter, Stats } from "./api.js";
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
