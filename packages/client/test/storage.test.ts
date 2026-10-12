import { afterEach, describe, expect, it, vi } from "vitest";
import { RxFerrixError, uploadFile } from "../src/index.js";
import { uploadRequestUrl } from "../src/storage.js";

const UPLOAD_URL = "http://localhost:3210/api/storage/upload?token=abc.def";

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

describe("uploadFile with fetch", () => {
  it("POSTs the raw body with its content type and file name, and returns the storage id", async () => {
    const fetch = vi.fn(async (_url: string | URL | Request, _init?: RequestInit) => json(200, { storageId: "k1" }));
    const progress: number[] = [];
    const file = new File(["hello"], "notes été.txt", { type: "text/plain" });
    const id = await uploadFile(UPLOAD_URL, file, {
      fetch: fetch as unknown as typeof globalThis.fetch,
      onProgress: (p) => progress.push(p.loaded / p.total),
    });
    expect(id).toBe("k1");
    expect(fetch).toHaveBeenCalledTimes(1);
    const [url, init] = fetch.mock.calls[0]!;
    const u = new URL(String(url));
    expect(u.pathname).toBe("/api/storage/upload");
    expect(u.searchParams.get("token")).toBe("abc.def");
    expect(u.searchParams.get("filename")).toBe("notes été.txt");
    expect(init?.method).toBe("POST");
    expect(init?.body).toBe(file);
    expect(init?.headers).toEqual({ "Content-Type": "text/plain" });
    expect(progress).toEqual([0, 1]);
  });

  it("sends no content type or name for an untyped Blob unless given", async () => {
    const fetch = vi.fn(async (_url: string | URL | Request, _init?: RequestInit) => json(200, { storageId: "k2" }));
    const f = fetch as unknown as typeof globalThis.fetch;
    await uploadFile(UPLOAD_URL, new Blob([new Uint8Array([1, 2, 3])]), { fetch: f });
    expect(new URL(String(fetch.mock.calls[0]![0])).searchParams.has("filename")).toBe(false);
    expect(fetch.mock.calls[0]![1]?.headers).toEqual({});
    await uploadFile(UPLOAD_URL, new Blob(["x"]), { fetch: f, contentType: "image/png", filename: "a.png" });
    expect(fetch.mock.calls[1]![1]?.headers).toEqual({ "Content-Type": "image/png" });
    expect(new URL(String(fetch.mock.calls[1]![0])).searchParams.get("filename")).toBe("a.png");
    // filename: null suppresses File.name.
    await uploadFile(UPLOAD_URL, new File(["x"], "secret.txt"), { fetch: f, filename: null });
    expect(new URL(String(fetch.mock.calls[2]![0])).searchParams.has("filename")).toBe(false);
  });

  it("resolves root-relative URLs against baseUrl", async () => {
    expect(uploadRequestUrl("/api/storage/upload?token=t", "x y.png", "http://h:1/app/")).toBe(
      "http://h:1/api/storage/upload?token=t&filename=x+y.png",
    );
    await expect(uploadFile("/api/storage/upload?token=t", new Blob(["x"]), { transport: "fetch" })).rejects.toMatchObject({
      code: "invalid_upload_url",
    });
  });

  it("turns server errors into RxFerrixErrors with their code", async () => {
    const fetch = vi.fn(async () =>
      json(413, { status: "error", errorMessage: "Files may be at most 10 bytes", errorData: { code: "file_too_large", data: null } }),
    );
    const err = await uploadFile(UPLOAD_URL, new Blob(["x".repeat(20)]), {
      fetch: fetch as unknown as typeof globalThis.fetch,
    }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(RxFerrixError);
    expect((err as RxFerrixError).code).toBe("file_too_large");
    expect((err as RxFerrixError).message).toBe("Files may be at most 10 bytes");

    const html = vi.fn(async () => new Response("<h1>Bad gateway</h1>", { status: 502 }));
    await expect(uploadFile(UPLOAD_URL, new Blob(["x"]), { fetch: html as unknown as typeof globalThis.fetch })).rejects.toMatchObject({
      code: "http_error",
    });
    const down = vi.fn(async () => {
      throw new TypeError("fetch failed");
    });
    await expect(uploadFile(UPLOAD_URL, new Blob(["x"]), { fetch: down as unknown as typeof globalThis.fetch })).rejects.toMatchObject({
      code: "network_error",
    });
  });

  it("can be aborted", async () => {
    const controller = new AbortController();
    controller.abort();
    const fetch = vi.fn();
    await expect(
      uploadFile(UPLOAD_URL, new Blob(["x"]), { fetch: fetch as unknown as typeof globalThis.fetch, signal: controller.signal }),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(fetch).not.toHaveBeenCalled();
  });
});

/** A scriptable XMLHttpRequest stand-in. */
class FakeXhr {
  static last: FakeXhr | undefined;
  method = "";
  url = "";
  headers: Record<string, string> = {};
  body: unknown;
  status = 0;
  responseText = "";
  upload: { onprogress: ((e: ProgressEvent) => void) | null } = { onprogress: null };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  aborted = false;

  constructor() {
    FakeXhr.last = this;
  }
  open(method: string, url: string) {
    this.method = method;
    this.url = url;
  }
  setRequestHeader(name: string, value: string) {
    this.headers[name] = value;
  }
  send(body: unknown) {
    this.body = body;
  }
  abort() {
    this.aborted = true;
    this.onabort?.();
  }
  // Test controls.
  progress(loaded: number, total: number) {
    this.upload.onprogress?.({ loaded, total, lengthComputable: true } as ProgressEvent);
  }
  respond(status: number, body: unknown) {
    this.status = status;
    this.responseText = JSON.stringify(body);
    this.onload?.();
  }
}

describe("uploadFile with XMLHttpRequest", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    FakeXhr.last = undefined;
  });

  it("is used when available and reports progress", async () => {
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    const progress: [number, number][] = [];
    const file = new File(["0123456789"], "clip.mp4", { type: "video/mp4" });
    const pending = uploadFile(UPLOAD_URL, file, { onProgress: (p) => progress.push([p.loaded, p.total]) });
    const xhr = FakeXhr.last!;
    expect(xhr.method).toBe("POST");
    expect(new URL(xhr.url).searchParams.get("filename")).toBe("clip.mp4");
    expect(xhr.headers).toEqual({ "Content-Type": "video/mp4" });
    expect(xhr.body).toBe(file);
    xhr.progress(4, 10);
    xhr.progress(10, 10);
    xhr.respond(200, { storageId: "k9" });
    await expect(pending).resolves.toBe("k9");
    expect(progress).toEqual([
      [4, 10],
      [10, 10],
      [10, 10],
    ]);
  });

  it("rejects with the server's error code, on network errors, and on abort", async () => {
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    const used = uploadFile(UPLOAD_URL, new Blob(["x"]));
    FakeXhr.last!.respond(403, {
      status: "error",
      errorMessage: "The upload URL has already been used",
      errorData: { code: "upload_url_used", data: null },
    });
    await expect(used).rejects.toMatchObject({ code: "upload_url_used" });

    const network = uploadFile(UPLOAD_URL, new Blob(["x"]));
    FakeXhr.last!.onerror?.();
    await expect(network).rejects.toMatchObject({ code: "network_error" });

    const controller = new AbortController();
    const aborted = uploadFile(UPLOAD_URL, new Blob(["x"]), { signal: controller.signal });
    controller.abort();
    await expect(aborted).rejects.toMatchObject({ name: "AbortError" });
    expect(FakeXhr.last!.aborted).toBe(true);
  });

  it("can be forced to fetch", async () => {
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    const fetch = vi.fn(async () => json(200, { storageId: "k3" }));
    await expect(
      uploadFile(UPLOAD_URL, new Blob(["x"]), { transport: "fetch", fetch: fetch as unknown as typeof globalThis.fetch }),
    ).resolves.toBe("k3");
    expect(FakeXhr.last).toBeUndefined();
  });
});
