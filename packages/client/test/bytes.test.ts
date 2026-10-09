import { describe, expect, it } from "vitest";
import { bytesFromJson, bytesToJson, isBytes, type Bytes } from "../src/index.js";

describe("bytes", () => {
  it("encodes like ferrix::Bytes (standard, padded base64)", () => {
    expect(bytesToJson(new Uint8Array([]))).toEqual({ $bytes: "" });
    expect(bytesToJson(new Uint8Array([0, 255]))).toEqual({ $bytes: "AP8=" });
    expect(bytesToJson(new Uint8Array([1, 2, 3]))).toEqual({ $bytes: "AQID" });
    expect(bytesToJson(new TextEncoder().encode("hi"))).toEqual({ $bytes: "aGk=" });
    expect(bytesToJson(new Uint8Array([0xaa, 0xbb]).buffer)).toEqual({ $bytes: "qrs=" });
    const view = new DataView(new Uint8Array([9, 1, 2, 9]).buffer, 1, 2);
    expect(bytesToJson(view)).toEqual({ $bytes: "AQI=" });
  });

  it("round-trips any bytes, including large ones", () => {
    const big = new Uint8Array(200_000).map((_, i) => (i * 31) % 256);
    expect(bytesFromJson(bytesToJson(big))).toEqual(big);
    const all = new Uint8Array(256).map((_, i) => i);
    expect(bytesFromJson(bytesToJson(all))).toEqual(all);
  });

  it("recognizes and rejects shapes", () => {
    expect(isBytes({ $bytes: "AA==" })).toBe(true);
    expect(isBytes({ $bytes: "AA==", x: 1 })).toBe(false);
    expect(isBytes({ $bytes: 1 })).toBe(false);
    expect(isBytes("AA==")).toBe(false);
    expect(isBytes(null)).toBe(false);
    expect(() => bytesFromJson({ $bytes: "%%%" })).toThrow(TypeError);
    expect(() => bytesFromJson("AA==" as unknown as Bytes)).toThrow(TypeError);
  });
});
