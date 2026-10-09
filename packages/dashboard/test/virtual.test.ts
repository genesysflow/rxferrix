import { describe, expect, it } from "vitest";
import { columnWidths, nearEnd, rowWindow, scrollToRow } from "../src/lib/virtual.js";

describe("rowWindow", () => {
  const base = { viewportHeight: 320, rowHeight: 32, count: 10_000, overscan: 5 };

  it("renders the rows in view plus overscan, with spacers for the rest", () => {
    const w = rowWindow({ ...base, scrollTop: 0 });
    // 10 rows fit, +1 for a partial row, +5 overscan below; nothing above.
    expect(w).toEqual({ start: 0, end: 16, padTop: 0, padBottom: (10_000 - 16) * 32 });

    const mid = rowWindow({ ...base, scrollTop: 32 * 1000 + 10 });
    expect(mid.start).toBe(995);
    expect(mid.end).toBe(1000 + 11 + 5);
    expect(mid.padTop).toBe(995 * 32);
    // The spacers and the rendered rows always add up to the full height.
    expect(mid.padTop + (mid.end - mid.start) * 32 + mid.padBottom).toBe(10_000 * 32);
  });

  it("stays smooth at the end and clamps overscrolled positions", () => {
    const end = rowWindow({ ...base, scrollTop: 10_000 * 32 - 320 });
    expect(end.end).toBe(10_000);
    expect(end.padBottom).toBe(0);
    const over = rowWindow({ ...base, scrollTop: 10_000_000 });
    expect(over.end).toBe(10_000);
    expect(over.start).toBeLessThan(over.end);
    const negative = rowWindow({ ...base, scrollTop: -50 });
    expect(negative.start).toBe(0);
  });

  it("keeps the rendered rows bounded however many rows are loaded", () => {
    for (const count of [100, 10_000, 1_000_000]) {
      for (const scrollTop of [0, 12_345, count * 16]) {
        const w = rowWindow({ ...base, count, scrollTop });
        expect(w.end - w.start).toBeLessThanOrEqual(10 + 1 + 2 * 5);
        expect(w.padTop + (w.end - w.start) * 32 + w.padBottom).toBe(count * 32);
      }
    }
  });

  it("handles empty lists and short lists", () => {
    expect(rowWindow({ ...base, count: 0, scrollTop: 0 })).toEqual({ start: 0, end: 0, padTop: 0, padBottom: 0 });
    expect(rowWindow({ ...base, count: 3, scrollTop: 0 })).toEqual({ start: 0, end: 3, padTop: 0, padBottom: 0 });
  });
});

describe("nearEnd", () => {
  it("asks for the next page when the window gets close to the loaded end", () => {
    expect(nearEnd({ start: 0, end: 20, padTop: 0, padBottom: 0 }, 200, 50)).toBe(false);
    expect(nearEnd({ start: 130, end: 151, padTop: 0, padBottom: 0 }, 200, 50)).toBe(true);
    expect(nearEnd({ start: 0, end: 0, padTop: 0, padBottom: 0 }, 0)).toBe(true);
  });
});

describe("scrollToRow", () => {
  it("scrolls the least needed to show a row", () => {
    expect(scrollToRow(5, 0, 320, 32)).toBe(0); // already visible
    expect(scrollToRow(20, 0, 320, 32)).toBe(21 * 32 - 320); // below: align its bottom
    expect(scrollToRow(2, 640, 320, 32)).toBe(64); // above: align its top
  });
});

describe("columnWidths", () => {
  it("sizes columns from the longest value, clamped", () => {
    const rows = [
      { _id: "0000022dbsg2b0typca0183asm", text: "short", n: 1, long: "x".repeat(500) },
      { _id: "0000023antbygce9ykv2nre8c0", text: "a somewhat longer text", n: 123456 },
    ];
    const [id, text, n, long, missing] = columnWidths(["_id", "text", "n", "long", "missing"], rows, {
      charWidth: 10,
      padding: 0,
      min: 50,
      max: 300,
    });
    expect(id).toBe(260);
    expect(text).toBe(220);
    expect(n).toBe(60);
    expect(long).toBe(300);
    expect(missing).toBe(70); // the header's length
  });
});
