import { describe, expect, it } from "vitest";
import { href, parseRoute } from "../src/lib/router.svelte.js";

describe("router", () => {
  it("parses dashboard paths", () => {
    expect(parseRoute("/_/", "")).toMatchObject({ page: "overview", rest: [] });
    const r = parseRoute("/_/data/todos", "?doc=abc");
    expect(r.page).toBe("data");
    expect(r.rest).toEqual(["todos"]);
    expect(r.params.get("doc")).toBe("abc");
    expect(parseRoute("/_/setup", "").page).toBe("setup");
    const s = parseRoute("/_/scheduler", "?state=failed");
    expect(s.page).toBe("scheduler");
    expect(s.params.get("state")).toBe("failed");
    expect(parseRoute("/_/nonsense", "").page).toBe("overview");
  });

  it("builds links", () => {
    expect(href("overview")).toBe("/_/");
    expect(href("data", ["my table"], { doc: "x", other: null })).toBe("/_/data/my%20table?doc=x");
    expect(href("functions", [], { fn: "todos:add" })).toBe("/_/functions?fn=todos%3Aadd");
  });
});
