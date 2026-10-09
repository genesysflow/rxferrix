import type { FerrixAuth } from "@ferrix/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createBackup,
  exportTable,
  fetchTimeseries,
  filenameFromDisposition,
  impersonate,
  importTable,
  saveBlob,
} from "../src/lib/admin.js";

/** A fake auth whose tokens are `t0`, `t1`, …: a new one on each forced refresh. */
function fakeAuth() {
  let n = 0;
  return {
    getAccessToken: vi.fn(async ({ forceRefresh = false } = {}) => `t${forceRefresh ? ++n : n}`),
  } as unknown as FerrixAuth;
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
    expect(filenameFromDisposition("attachment; filename=ferrix-1.tar.zst", "x")).toBe("ferrix-1.tar.zst");
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
    expect(url).toBe("/api/admin/import/todos?mode=upsert");
    expect(init?.method).toBe("POST");
    expect(header(init, "content-type")).toBe("application/x-ndjson");
  });

  it("creates backups with the options", async () => {
    const fetch = mockFetch(new Response(JSON.stringify({ name: "ferrix-1.tar.zst", bytes: 10, createdAt: 1, scheduled: false, ts: 3 })));
    const b = await createBackup(fakeAuth(), { includeAux: true, includeFiles: false });
    expect(b.name).toBe("ferrix-1.tar.zst");
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
          url: "http://app/#ferrix_access_token=secret-access",
        }),
      ),
    );
    const r = await impersonate(fakeAuth(), { user: "u1", ttlSeconds: 300, redirectTo: "http://app/" });
    expect(r).toEqual({ userId: "u1", sessionId: "s1", expiresAt: 123, url: "http://app/#ferrix_access_token=secret-access" });
    expect(Object.keys(r)).not.toContain("accessToken");
    expect(Object.keys(r)).not.toContain("refreshToken");
  });
});
