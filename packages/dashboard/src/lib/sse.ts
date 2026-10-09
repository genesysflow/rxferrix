/**
 * A minimal Server-Sent Events parser for `fetch()` response bodies. The dashboard reads `/api/admin/logs/stream`
 * with `fetch` instead of `EventSource` so it can send `Authorization: Bearer` (EventSource can't set headers, and
 * putting the token in the URL would leak it into logs).
 */
export interface SseEvent {
  event: string;
  data: string;
}

/** Feed text chunks in; get complete events out. Handles `\n`, `\r\n` and `\r` line ends and multi-line data. */
export class SseParser {
  private buffer = "";
  private event = "";
  private data: string[] = [];

  push(chunk: string): SseEvent[] {
    this.buffer += chunk;
    const out: SseEvent[] = [];
    for (;;) {
      const match = /\r\n|\r|\n/.exec(this.buffer);
      // A lone trailing "\r" may be the first half of "\r\n": wait for more input.
      if (!match || (match[0] === "\r" && match.index === this.buffer.length - 1)) break;
      const line = this.buffer.slice(0, match.index);
      this.buffer = this.buffer.slice(match.index + match[0].length);
      if (line === "") {
        if (this.data.length > 0 || this.event !== "") {
          out.push({ event: this.event || "message", data: this.data.join("\n") });
        }
        this.event = "";
        this.data = [];
        continue;
      }
      if (line.startsWith(":")) continue; // comment (keep-alive)
      const colon = line.indexOf(":");
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? "" : line.slice(colon + 1);
      if (value.startsWith(" ")) value = value.slice(1);
      if (field === "event") this.event = value;
      else if (field === "data") this.data.push(value);
    }
    return out;
  }
}

/** Read the SSE events of a fetch response until it ends or `signal` aborts. */
export async function* readSse(response: Response, signal?: AbortSignal): AsyncGenerator<SseEvent> {
  if (!response.body) return;
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  const parser = new SseParser();
  const onAbort = () => void reader.cancel().catch(() => {});
  signal?.addEventListener("abort", onAbort);
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return;
      for (const e of parser.push(decoder.decode(value, { stream: true }))) yield e;
    }
  } finally {
    signal?.removeEventListener("abort", onAbort);
    reader.releaseLock();
  }
}
