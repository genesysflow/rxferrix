import type { RxFerrixAuth } from "@rxferrix/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cancelRestore,
  createBackup,
  exportTable,
  fetchTimeseries,
  filenameFromDisposition,
  getReplication,
  impersonate,
  importFormatOf,
  importTable,
  listBackups,
  replicationSnapshot,
  restartServer,
  saveBlob,
  stagePointInTime,
  stageRestore,
  uploadRestore,
  waitForRestart,
} from "../src/lib/admin.js";

/** A fake auth whose tokens are `t0`, `t1`, …: a new one on each forced refresh. */
function fakeAuth() {
  let n = 0;
  return {
    getAccessToken: vi.fn(async ({ forceRefresh = false } = {}) => `t${forceRefresh ? ++n : n}`),
  } as unknown as RxFerrixAuth;
}

function mockFetch(...responses: Response[]) {
  const fetch = vi.fn(async (_url: string, _init?: RequestInit) => responses.shift() ?? new Response(null, { status: 500 }));
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

const header = (init: RequestInit | undefined, name: string) => new Headers(init?.headers).get(name);

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("filenameFromDisposition", () => {
  it("reads quoted, plain and RFC 5987 names, never a path", () => {
    expect(filenameFromDisposition('attachment; filename="todos.jsonl"', "x")).toBe("todos.jsonl");
    expect(filenameFromDisposition("attachment; filename=rxferrix-1.tar.zst", "x")).toBe("rxferrix-1.tar.zst");
    expect(filenameFromDisposition(`attachment; filename="a.txt"; filename*=UTF-8''r%C3%A9sum%C3%A9.txt`, "x")).toBe("résumé.txt");
    expect(filenameFromDisposition('attachment; filename="../../etc/passwd"', "x")).toBe(".._.._etc_passwd");
    expect(filenameFromDisposition(null, "fallback.csv")).toBe("fallback.csv");
    expect(filenameFromDisposition("attachment", "fallback.csv")).toBe("fallback.csv");
  });
});

describe("export download", () => {
  it("fetches with the bearer token (not in the URL) and keeps the server's file name", async () => {
    const fetch = mockFetch(
      new Response('{"_id":"1"}\n', { headers: { "content-disposition": 'attachment; filename="todos.jsonl"' } }),
    );
    const d = await exportTable(fakeAuth(), "todos", "jsonl");
    const [url, init] = fetch.mock.calls[0]!;
    expect(url).toBe("/api/admin/export/todos?format=jsonl");
    expect(header(init, "authorization")).toBe("Bearer t0");
    expect(d.filename).toBe("todos.jsonl");
    expect(await d.blob.text()).toBe('{"_id":"1"}\n');
  });

  it("retries once with a refreshed token on 401, and surfaces server errors", async () => {
    const fetch = mockFetch(
      new Response(null, { status: 401 }),
      new Response("a,b\r\n", { headers: { "content-type": "text/csv" } }),
    );
    const d = await exportTable(fakeAuth(), "my table", "csv");
    expect(fetch.mock.calls[0]![0]).toBe("/api/admin/export/my%20table?format=csv");
    expect(header(fetch.mock.calls[1]![1], "authorization")).toBe("Bearer t1");
    expect(d.filename).toBe("my table.csv");

    mockFetch(
      new Response(JSON.stringify({ errorMessage: "No table `nope`", errorData: { code: "not_found" } }), { status: 404 }),
    );
    await expect(exportTable(fakeAuth(), "nope", "jsonl")).rejects.toMatchObject({ message: "No table `nope`", code: "not_found" });
  });

  it("saves a blob through a temporary download link", () => {
    const createObjectURL = vi.fn(() => "blob:fake");
    const revokeObjectURL = vi.fn();
    vi.stubGlobal("URL", Object.assign(URL, { createObjectURL, revokeObjectURL }));
    const clicks: HTMLAnchorElement[] = [];
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      clicks.push(this);
    });
    vi.useFakeTimers();
    try {
      const blob = new Blob(["x"]);
      saveBlob({ blob, filename: "todos.csv" });
      expect(createObjectURL).toHaveBeenCalledWith(blob);
      expect(clicks).toHaveLength(1);
      expect(clicks[0]!.download).toBe("todos.csv");
      expect(clicks[0]!.getAttribute("href")).toBe("blob:fake");
      expect(document.querySelector("a[download]")).toBeNull(); // removed again
      vi.runAllTimers();
      expect(revokeObjectURL).toHaveBeenCalledWith("blob:fake");
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("other admin endpoints", () => {
  it("imports JSONL with the mode", async () => {
    const report = { tables: [{ table: "todos", documents: 2, replaced: 0, deleted: 0 }], files: 0, missingFiles: 0, remappedIds: false };
    const fetch = mockFetch(new Response(JSON.stringify(report)));
    expect(await importTable(fakeAuth(), "todos", "upsert", "{}\n{}\n")).toEqual(report);
    const [url, init] = fetch.mock.calls[0]!;
    expect(url).toBe("/api/admin/import/todos?mode=upsert&format=jsonl");
    expect(init?.method).toBe("POST");
    expect(header(init, "content-type")).toBe("application/x-ndjson");
  });

  it("imports CSV with the format, inferred from the file name", async () => {
    const report = { tables: [], files: 0, missingFiles: 0, remappedIds: false };
    const fetch = mockFetch(new Response(JSON.stringify(report)));
    await importTable(fakeAuth(), "todos", "insert", "_id,text\r\n", importFormatOf("todos.CSV"));
    const [url, init] = fetch.mock.calls[0]!;
    expect(url).toBe("/api/admin/import/todos?mode=insert&format=csv");
    expect(header(init, "content-type")).toBe("text/csv");
    expect(importFormatOf("todos.jsonl")).toBe("jsonl");
    expect(importFormatOf("data.ndjson")).toBe("jsonl");
  });

  it("stages, uploads, cancels and restarts restores", async () => {
    const state = { pendingRestore: { source: "rxferrix-1.tar.zst" }, restart: { available: true, exitCode: 75 } };
    const fetch = mockFetch(
      new Response(JSON.stringify(state)),
      new Response(JSON.stringify(state)),
      new Response(JSON.stringify({ canceled: true })),
      new Response(JSON.stringify({ restarting: true, exitCode: 75 }), { status: 202 }),
    );
    expect(await stageRestore(fakeAuth(), "rxferrix-1.tar.zst")).toEqual(state);
    expect(fetch.mock.calls[0]![0]).toBe("/api/admin/backups/rxferrix-1.tar.zst/restore");
    expect(fetch.mock.calls[0]![1]?.method).toBe("POST");
    const file = new Blob([new Uint8Array([0x28, 0xb5, 0x2f, 0xfd])]);
    await uploadRestore(fakeAuth(), file, "my backup.tar.zst");
    expect(fetch.mock.calls[1]![0]).toBe("/api/admin/restore?name=my%20backup.tar.zst");
    expect(header(fetch.mock.calls[1]![1], "content-type")).toBe("application/zstd");
    expect(fetch.mock.calls[1]![1]?.body).toBe(file);
    await cancelRestore(fakeAuth());
    expect(fetch.mock.calls[2]![1]?.method).toBe("DELETE");
    expect(await restartServer(fakeAuth())).toEqual({ restarting: true, exitCode: 75 });
    expect(fetch.mock.calls[3]![0]).toBe("/api/admin/restore/restart");
  });

  it("reports a refused restore with the server's reason", async () => {
    const body = { status: "error", errorMessage: "This backup can't be restored: corrupt", errorData: { code: "invalid_backup" } };
    mockFetch(new Response(JSON.stringify(body), { status: 400 }));
    await expect(stageRestore(fakeAuth(), "rxferrix-1.tar.zst")).rejects.toThrow("This backup can't be restored: corrupt");
  });

  it("lists backups with the restore state (defaults for older servers)", async () => {
    mockFetch(new Response(JSON.stringify({ target: "/data/backups", backups: [] })));
    expect(await listBackups(fakeAuth())).toEqual({
      target: "/data/backups",
      backups: [],
      pendingRestore: null,
      restart: { available: false, exitCode: null },
      replication: null,
    });
  });

  it("reads the replication status, snapshots and stages point-in-time restores", async () => {
    const status = { target: "s3://b/r", generation: "g", lagCommits: 0, generations: [] };
    const state = { pendingRestore: { source: "point in time: ts 7 of generation g" }, restart: { available: false, exitCode: null } };
    const fetch = mockFetch(
      new Response(JSON.stringify({ enabled: false })),
      new Response(JSON.stringify({ enabled: true, ...status })),
      new Response(JSON.stringify({ name: "rxferrix-auto-x-ts9.tar.zst", ts: 9, createdAt: 1, bytes: 2 }), { status: 201 }),
      new Response(JSON.stringify(state)),
    );
    expect(await getReplication(fakeAuth())).toBeNull();
    expect(await getReplication(fakeAuth())).toEqual(status);
    expect(fetch.mock.calls[1]![0]).toBe("/api/admin/replication");
    expect((await replicationSnapshot(fakeAuth())).ts).toBe(9);
    expect(fetch.mock.calls[2]![1]?.method).toBe("POST");
    expect(await stagePointInTime(fakeAuth(), { toTs: 7, generation: "g" })).toEqual(state);
    const [url, init] = fetch.mock.calls[3]!;
    expect(url).toBe("/api/admin/restore/point-in-time");
    expect(JSON.parse(init!.body as string)).toEqual({ toTs: 7, generation: "g" });
  });

  it("waits for the server to go down and come back", async () => {
    let n = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        n++;
        if (n === 1) return new Response("ok");
        if (n === 2) throw new TypeError("connection refused");
        return new Response("ok");
      }),
    );
    expect(await waitForRestart(5_000, 1)).toBe(true);
    expect(n).toBe(3);
    vi.stubGlobal("fetch", vi.fn(async () => new Response("ok")));
    expect(await waitForRestart(20, 1)).toBe(false);
  });

  it("creates backups with the options", async () => {
    const fetch = mockFetch(new Response(JSON.stringify({ name: "rxferrix-1.tar.zst", bytes: 10, createdAt: 1, scheduled: false, ts: 3 })));
    const b = await createBackup(fakeAuth(), { includeAux: true, includeFiles: false });
    expect(b.name).toBe("rxferrix-1.tar.zst");
    expect(JSON.parse(fetch.mock.calls[0]![1]!.body as string)).toEqual({ includeAux: true, includeFiles: false });
  });

  it("asks for a time series", async () => {
    const fetch = mockFetch(new Response(JSON.stringify({ path: "a:b", bucket: 60000, since: 0, until: 1, points: [] })));
    await fetchTimeseries(fakeAuth(), { path: "a:b", bucket: 60_000, since: 1.7, until: 120_000.2 });
    expect(fetch.mock.calls[0]![0]).toBe("/api/admin/stats/timeseries?bucket=60000&since=1&path=a%3Ab&until=120001");
  });

  it("keeps no tokens from an impersonation", async () => {
    mockFetch(
      new Response(
        JSON.stringify({
          accessToken: "secret-access",
          refreshToken: "secret-refresh",
          expiresIn: 900,
          userId: "u1",
          sessionId: "s1",
          expiresAt: 123,
          url: "http://app/#rxferrix_access_token=secret-access",
        }),
      ),
    );
    const r = await impersonate(fakeAuth(), { user: "u1", ttlSeconds: 300, redirectTo: "http://app/" });
    expect(r).toEqual({ userId: "u1", sessionId: "s1", expiresAt: 123, url: "http://app/#rxferrix_access_token=secret-access" });
    expect(Object.keys(r)).not.toContain("accessToken");
    expect(Object.keys(r)).not.toContain("refreshToken");
  });
});
