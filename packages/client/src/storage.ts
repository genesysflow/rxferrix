/**
 * File uploads to a RxFerrix server.
 *
 * The flow: a mutation returns `ctx.storage.generate_upload_url()`, the client `POST`s the file to it
 * with {@link uploadFile}, and a second mutation stores the returned `Id<"_storage">` in a document. Serve it with
 * the URL from `ctx.storage.get_url(id)` (`/api/storage/<id>`).
 *
 * ```ts
 * const url = await client.mutation(api.files.generateUploadUrl, {});
 * const storageId = await uploadFile(url, file, { onProgress: ({ loaded, total }) => (pct = loaded / total) });
 * await client.mutation(api.files.attach, { storageId });
 * ```
 */
import type { Id } from "./api.js";
import { RxFerrixError } from "./errors.js";
import type { ErrorData } from "./protocol.js";

export interface UploadProgress {
  /** Bytes sent so far. */
  loaded: number;
  /** Total bytes to send. */
  total: number;
}

export interface UploadFileOptions {
  /** Called as the upload progresses (with `XMLHttpRequest`; with `fetch`, only at the start and the end). */
  onProgress?: (progress: UploadProgress) => void;
  /** Aborts the upload; the promise rejects with an `AbortError`. */
  signal?: AbortSignal;
  /** The `Content-Type` to store. Default: the Blob's `type` (none if empty). */
  contentType?: string;
  /** The original file name to store. Default: `File.name` for a `File`; `null` sends none. */
  filename?: string | null;
  /** Base for a root-relative upload URL (`/api/storage/upload?…`). Default: the page's `location`. */
  baseUrl?: string;
  /** `fetch` to use when `XMLHttpRequest` is unavailable (or `transport: "fetch"`). Default: `globalThis.fetch`. */
  fetch?: typeof globalThis.fetch;
  /** Force a transport. Default: `XMLHttpRequest` when available (browsers, for progress events), else `fetch`. */
  transport?: "xhr" | "fetch";
}

/** A successful upload's response body. */
interface UploadResponse {
  storageId: string;
}

/** The final URL: `uploadUrl` resolved against `baseUrl`, with `filename` appended. */
export function uploadRequestUrl(uploadUrl: string, filename: string | null | undefined, baseUrl?: string): string {
  const base = baseUrl ?? (typeof location !== "undefined" ? location.href : undefined);
  let url: URL;
  try {
    url = base === undefined ? new URL(uploadUrl) : new URL(uploadUrl, base);
  } catch {
    throw new RxFerrixError(`Invalid upload URL ${JSON.stringify(uploadUrl)} (pass options.baseUrl for relative URLs)`, {
      code: "invalid_upload_url",
    });
  }
  if (filename) url.searchParams.set("filename", filename);
  return url.toString();
}

function parseResult(status: number, text: string): Id<"_storage"> {
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    // Not JSON (e.g. a proxy's error page).
  }
  if (status >= 200 && status < 300) {
    const id = (body as UploadResponse | null)?.storageId;
    if (typeof id !== "string") throw new RxFerrixError(`Unexpected upload response: ${text.slice(0, 200)}`);
    return id as Id<"_storage">;
  }
  const err = body as { errorMessage?: string; errorData?: ErrorData | null } | null;
  if (err && typeof err === "object" && (err.errorMessage || err.errorData)) {
    throw RxFerrixError.fromServer(err.errorMessage, err.errorData ?? null);
  }
  throw new RxFerrixError(`Upload failed with HTTP ${status}`, { code: status === 413 ? "file_too_large" : "http_error" });
}

function abortError(signal: AbortSignal | undefined): unknown {
  return signal?.reason ?? new DOMException("The upload was aborted", "AbortError");
}

/**
 * Upload `file` to an upload URL from `ctx.storage.generate_upload_url()` and return the new file's
 * `Id<"_storage">`. Rejects with a {@link RxFerrixError} whose `code` is the server's (`file_too_large`,
 * `upload_url_expired`, `upload_url_used`, `invalid_upload_url`, `rate_limited`, ...), or `network_error`.
 */
export async function uploadFile(uploadUrl: string, file: Blob, options: UploadFileOptions = {}): Promise<Id<"_storage">> {
  const filename =
    options.filename !== undefined
      ? options.filename
      : typeof File !== "undefined" && file instanceof File && file.name
        ? file.name
        : null;
  const url = uploadRequestUrl(uploadUrl, filename, options.baseUrl);
  const contentType = options.contentType ?? (file.type || undefined);
  if (options.signal?.aborted) throw abortError(options.signal);
  const useXhr = options.transport ? options.transport === "xhr" : typeof XMLHttpRequest === "function";
  return useXhr ? uploadWithXhr(url, file, contentType, options) : uploadWithFetch(url, file, contentType, options);
}

function uploadWithXhr(
  url: string,
  file: Blob,
  contentType: string | undefined,
  options: UploadFileOptions,
): Promise<Id<"_storage">> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const { signal, onProgress } = options;
    const onAbort = () => xhr.abort();
    const cleanup = () => signal?.removeEventListener("abort", onAbort);
    xhr.open("POST", url);
    if (contentType) xhr.setRequestHeader("Content-Type", contentType);
    if (onProgress) {
      xhr.upload.onprogress = (e: ProgressEvent) => {
        onProgress({ loaded: e.loaded, total: e.lengthComputable ? e.total : file.size });
      };
    }
    xhr.onload = () => {
      cleanup();
      try {
        const id = parseResult(xhr.status, xhr.responseText);
        onProgress?.({ loaded: file.size, total: file.size });
        resolve(id);
      } catch (e) {
        reject(e);
      }
    };
    xhr.onerror = () => {
      cleanup();
      reject(new RxFerrixError("The upload failed: network error", { code: "network_error" }));
    };
    xhr.onabort = () => {
      cleanup();
      reject(abortError(signal));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
    xhr.send(file);
  });
}

async function uploadWithFetch(
  url: string,
  file: Blob,
  contentType: string | undefined,
  options: UploadFileOptions,
): Promise<Id<"_storage">> {
  const fetchFn = options.fetch ?? globalThis.fetch;
  options.onProgress?.({ loaded: 0, total: file.size });
  let res: Response;
  try {
    res = await fetchFn(url, {
      method: "POST",
      body: file,
      headers: contentType ? { "Content-Type": contentType } : {},
      signal: options.signal,
    });
  } catch (e) {
    if (options.signal?.aborted) throw abortError(options.signal);
    throw new RxFerrixError(`The upload failed: ${e instanceof Error ? e.message : String(e)}`, {
      code: "network_error",
      cause: e,
    });
  }
  const id = parseResult(res.status, await res.text());
  options.onProgress?.({ loaded: file.size, total: file.size });
  return id;
}
