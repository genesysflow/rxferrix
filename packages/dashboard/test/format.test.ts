import { describe, expect, it } from "vitest";
import {
  columnsOf,
  compactJson,
  formatBytes,
  formatDuration,
  formatRelative,
  formatSpan,
  idTableNumber,
  parseFilterValue,
  parseJson,
  prettyArgs,
  previewKind,
  sameOriginPath,
} from "../src/lib/format.js";

const ALPHABET = "0123456789abcdefghjkmnpqrstvwxyz";

/** The Rust id encoding (crates/rxferrix/src/id.rs): Crockford base32 of [table number BE u32][12 bytes]. */
function makeId(table: number, fill = 0xab): string {
  const bytes = [table >>> 24, (table >>> 16) & 255, (table >>> 8) & 255, table & 255, ...Array(12).fill(fill)];
  let bits = "";
  for (const b of bytes) bits += b.toString(2).padStart(8, "0");
  bits = bits.padEnd(130, "0");
  let out = "";
  for (let i = 0; i < 130; i += 5) out += ALPHABET[parseInt(bits.slice(i, i + 5), 2)];
  return out;
}

describe("idTableNumber", () => {
  it("decodes the table number of real ids", () => {
    // Ids from a real server: todos is table 8 there (after the 7 auth system tables).
    expect(idTableNumber("0000022dbsg2b0typca0183asm")).toBe(8);
    expect(idTableNumber("0000023antbygce9ykv2nre8c0")).toBe(8);
    for (const n of [1, 7, 255, 256, 65_537, 0xffffffff]) expect(idTableNumber(makeId(n))).toBe(n);
  });

  it("rejects things that are not ids", () => {
    expect(idTableNumber("hello")).toBeNull();
    expect(idTableNumber(42)).toBeNull();
    expect(idTableNumber(makeId(3).toUpperCase())).toBeNull();
    expect(idTableNumber(`${makeId(3).slice(0, 25)}u`)).toBeNull(); // not in the alphabet
    expect(idTableNumber(`${makeId(3).slice(0, 25)}1`)).toBeNull(); // non-zero padding bits
  });
});

describe("formatting", () => {
  it("renders cells compactly", () => {
    expect(compactJson({ a: [1, 2] })).toBe('{"a":[1,2]}');
    expect(compactJson("x".repeat(100), 10)).toBe(`${"x".repeat(9)}…`);
    expect(compactJson(undefined)).toBe("");
  });

  it("orders columns: _id, user fields, _creationTime", () => {
    expect(columnsOf([{ _creationTime: 1, _id: "a", text: "x" }, { _id: "b", done: true, _creationTime: 2 }])).toEqual([
      "_id",
      "text",
      "done",
      "_creationTime",
    ]);
    expect(columnsOf([])).toEqual(["_id", "_creationTime"]);
  });

  it("parses filter values as JSON or plain text", () => {
    expect(parseFilterValue("42")).toBe(42);
    expect(parseFilterValue("true")).toBe(true);
    expect(parseFilterValue('"42"')).toBe("42");
    expect(parseFilterValue(" hello ")).toBe("hello");
    expect(parseFilterValue("null")).toBeNull();
  });

  it("formats durations, sizes and args", () => {
    expect(formatDuration(250)).toBe("250µs");
    expect(formatDuration(2500)).toBe("2.5ms");
    expect(formatDuration(2_500_000)).toBe("2.50s");
    expect(formatSpan(850)).toBe("850ms");
    expect(formatSpan(12_400)).toBe("12.4s");
    expect(formatSpan(185_000)).toBe("3m 05s");
    expect(formatSpan(7_800_000)).toBe("2h 10m");
    expect(formatSpan(3 * 86_400_000 + 4 * 3_600_000)).toBe("3d 4h");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(prettyArgs('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(prettyArgs('{"a":"trunc')).toBe('{"a":"trunc');
    expect(parseJson("{").ok).toBe(false);
  });

  it("formats times relative to now", () => {
    const now = 1_000_000_000;
    expect(formatRelative(now, now)).toBe("now");
    expect(formatRelative(now + 5_000, now)).toBe("in 5s");
    expect(formatRelative(now + 5 * 60_000, now)).toBe("in 5m");
    expect(formatRelative(now - 3 * 3600_000, now)).toBe("3h ago");
    expect(formatRelative(now + 2 * 86400_000, now)).toBe("in 2d");
  });
});

describe("files", () => {
  it("picks a preview kind from the content type", () => {
    expect(previewKind("image/png")).toBe("image");
    expect(previewKind("image/svg+xml")).toBe("image");
    expect(previewKind("VIDEO/mp4")).toBe("video");
    expect(previewKind("audio/ogg")).toBe("audio");
    expect(previewKind("text/plain; charset=utf-8")).toBe("text");
    expect(previewKind("application/json")).toBe("text");
    expect(previewKind("application/ld+json")).toBe("text");
    expect(previewKind("application/pdf")).toBe(null);
    expect(previewKind(null)).toBe(null);
  });

  it("makes server URLs same-origin", () => {
    expect(sameOriginPath("http://127.0.0.1:8090/api/storage/upload?token=a.b")).toBe("/api/storage/upload?token=a.b");
    expect(sameOriginPath("/api/storage/abc")).toBe("/api/storage/abc");
  });
});
