import { describe, expect, it } from "vitest";
import { filterQuery } from "../src/lib/admin.js";
import { readSse, SseParser } from "../src/lib/sse.js";

describe("SseParser", () => {
  it("parses events split across chunks, with comments and CRLF", () => {
    const p = new SseParser();
    expect(p.push(": keep-alive\n\nevent: ready\nda")).toEqual([]);
    expect(p.push("ta: {}\n\nevent: log\r\ndata: {\"id\":1}\r")).toEqual([{ event: "ready", data: "{}" }]);
    expect(p.push("\n\r\n")).toEqual([{ event: "log", data: '{"id":1}' }]);
  });

  it("joins multi-line data and defaults the event name", () => {
    const p = new SseParser();
    expect(p.push("data: a\ndata: b\n\n")).toEqual([{ event: "message", data: "a\nb" }]);
  });

  it("reads a fetch response body", async () => {
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        const enc = new TextEncoder();
        c.enqueue(enc.encode("event: ready\ndata: {}\n\nevent: lagged\n"));
        c.enqueue(enc.encode("data: 3\n\n"));
        c.close();
      },
    });
    const events = [];
    for await (const e of readSse(new Response(body))) events.push(e);
    expect(events).toEqual([
      { event: "ready", data: "{}" },
      { event: "lagged", data: "3" },
    ]);
  });
});

describe("filterQuery", () => {
  it("leaves out empty filters", () => {
    expect(filterQuery({ pathPrefix: "todos:", kind: "", outcome: "userError", excludeSystem: true, limit: 100 })).toBe(
      "?pathPrefix=todos%3A&outcome=userError&excludeSystem=true&limit=100",
    );
    expect(filterQuery({ excludeSystem: false })).toBe("");
  });
});
