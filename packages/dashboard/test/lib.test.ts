import { describe, expect, it } from "vitest";
import { fuzzyScore, rankCommands, type Command } from "../src/lib/commands.js";
import { diffFields, summarize } from "../src/lib/diff.js";
import { documentMatches, formatSeconds, hasThumbnail, thumbUrl, withParams } from "../src/lib/format.js";
import { parseRoute } from "../src/lib/router.svelte.js";

describe("withParams and thumbnails", () => {
  it("adds parameters to plain and signed URLs without a second `?`", () => {
    expect(withParams("/api/storage/abc", { download: 1 })).toBe("/api/storage/abc?download=1");
    expect(withParams("/api/storage/abc?exp=123&sig=x-y_z", { download: 1 })).toBe("/api/storage/abc?exp=123&sig=x-y_z&download=1");
    expect(withParams("https://h.example/api/storage/a?exp=1&sig=s#frag", { thumb: "96x96", fit: "cover" })).toBe(
      "https://h.example/api/storage/a?exp=1&sig=s&thumb=96x96&fit=cover#frag",
    );
    // Replaces and removes.
    expect(withParams("/x?a=1&b=2", { a: 3, b: null })).toBe("/x?a=3");
  });

  it("builds same-origin thumbnail URLs for the image types the server can scale", () => {
    expect(thumbUrl("http://public.example.com/api/storage/abc?exp=9&sig=s", 96)).toBe("/api/storage/abc?exp=9&sig=s&thumb=96x96&fit=cover");
    expect(thumbUrl("/api/storage/abc", 64, 48, "contain")).toBe("/api/storage/abc?thumb=64x48&fit=contain");
    expect(hasThumbnail("image/png")).toBe(true);
    expect(hasThumbnail("image/jpeg; q=1")).toBe(true);
    expect(hasThumbnail("image/svg+xml")).toBe(false);
    expect(hasThumbnail(null)).toBe(false);
  });
});

describe("format helpers", () => {
  it("formats durations in seconds", () => {
    expect(formatSeconds(45)).toBe("45s");
    expect(formatSeconds(90)).toBe("1m 30s");
    expect(formatSeconds(3600)).toBe("1h");
    expect(formatSeconds(2_592_000)).toBe("30d");
    expect(formatSeconds(90_061)).toBe("1d 1h");
    expect(formatSeconds(-1)).toBe("—");
  });

  it("matches documents on any value, nested included (the quick filter)", () => {
    const doc = { _id: "abc", text: "Buy MILK", tags: ["groceries"], meta: { by: "ada" }, n: 42, none: null };
    expect(documentMatches(doc, "milk")).toBe(true);
    expect(documentMatches(doc, "grocer")).toBe(true);
    expect(documentMatches(doc, "ada")).toBe(true);
    expect(documentMatches(doc, "42")).toBe(true);
    expect(documentMatches(doc, "text")).toBe(false); // keys don't match
    expect(documentMatches(doc, "null")).toBe(false);
    expect(documentMatches(doc, "")).toBe(true);
  });
});

describe("diffFields", () => {
  it("lists changed, added and removed fields, _id first and system fields last", () => {
    const changes = diffFields(
      { _id: "1", text: "a", done: false, gone: 1, _creationTime: 5 },
      { _id: "1", text: "b", done: false, added: [1], _creationTime: 5 },
    );
    expect(changes.map((c) => [c.field, c.kind])).toEqual([
      ["_id", "same"],
      ["added", "added"],
      ["done", "same"],
      ["gone", "removed"],
      ["text", "changed"],
      ["_creationTime", "same"],
    ]);
    expect(summarize(changes)).toBe("1 changed, 1 added, 1 removed");
  });

  it("handles inserts, deletes and non-object values", () => {
    expect(diffFields(null, { a: 1 })).toEqual([{ field: "a", kind: "added", before: undefined, after: 1 }]);
    expect(diffFields({ a: 1 }, null)).toEqual([{ field: "a", kind: "removed", before: 1, after: undefined }]);
    expect(diffFields(1, 2)).toEqual([{ field: "(value)", kind: "changed", before: 1, after: 2 }]);
    expect(diffFields(null, null)).toEqual([]);
    expect(summarize([])).toBe("no field changes");
  });
});

describe("command palette ranking", () => {
  const cmd = (label: string, keywords?: string): Command => ({ id: label, label, group: "Page", keywords, run: () => {} });
  const commands = [cmd("Overview"), cmd("Data"), cmd("Audit log", "history"), cmd("_audit_log"), cmd("Settings", "smtp oauth")];

  it("matches subsequences, preferring prefixes and word starts", () => {
    expect(fuzzyScore("aud", "Audit log")).not.toBeNull();
    expect(fuzzyScore("xyz", "Audit log")).toBeNull();
    expect(rankCommands("audit", commands).map((c) => c.label)).toEqual(["Audit log", "_audit_log"]);
    expect(rankCommands("al", commands).map((c) => c.label)[0]).toBe("Audit log");
  });

  it("matches keywords, and lists everything for an empty query", () => {
    expect(rankCommands("smtp", commands).map((c) => c.label)).toEqual(["Settings"]);
    expect(rankCommands("hist", commands).map((c) => c.label)).toEqual(["Audit log"]);
    expect(rankCommands("  ", commands)).toHaveLength(commands.length);
  });
});

describe("routes", () => {
  it("knows the new pages", () => {
    for (const page of ["migrations", "backups", "settings", "audit"] as const) {
      expect(parseRoute(`/_/${page}`, "").page).toBe(page);
    }
    expect(parseRoute("/_/audit", "?action=patch").params.get("action")).toBe("patch");
  });
});
