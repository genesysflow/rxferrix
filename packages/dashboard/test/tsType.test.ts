import { describe, expect, it } from "vitest";
import { defaultObject, fieldKind, kindOfValue, parseTsType, type TsType } from "../src/lib/tsType.js";

const fields = (t: TsType | null) => (t?.kind === "object" ? t.fields : null);

describe("parseTsType", () => {
  it("reads a table type with nullable fields and ids", () => {
    const t = parseTsType(`{ text: string, done: boolean, image: Id<"_storage"> | null, completedAt: number | null, }`);
    expect(fields(t)).toEqual([
      { name: "text", optional: false, type: { kind: "string" } },
      { name: "done", optional: false, type: { kind: "boolean" } },
      { name: "image", optional: false, type: { kind: "union", of: [{ kind: "id", table: "_storage" }, { kind: "null" }] } },
      { name: "completedAt", optional: false, type: { kind: "union", of: [{ kind: "number" }, { kind: "null" }] } },
    ]);
  });

  it("reads literal unions, nested objects, arrays, any and maps", () => {
    const t = parseTsType(
      `{ state: "running" | "success" | "failed", onComplete: { path: string, context: any } | null, ` +
        `events: Array<{ name: string, value: any }>, tags: string[], counts: { [key in string]?: number }, pair: [string, number], }`,
    );
    const f = fields(t)!;
    expect(f[0]!.type).toEqual({
      kind: "union",
      of: [
        { kind: "literal", value: "running" },
        { kind: "literal", value: "success" },
        { kind: "literal", value: "failed" },
      ],
    });
    expect(f[1]!.type).toEqual({
      kind: "union",
      of: [{ kind: "object", fields: [{ name: "path", optional: false, type: { kind: "string" } }, { name: "context", optional: false, type: { kind: "any" } }] }, { kind: "null" }],
    });
    expect(f[2]!.type.kind).toBe("array");
    expect(f[3]!.type).toEqual({ kind: "array", of: { kind: "string" } });
    expect(f[4]!.type).toEqual({ kind: "record", of: { kind: "number" } });
    expect(f[5]!.type).toEqual({ kind: "tuple", of: [{ kind: "string" }, { kind: "number" }] });
  });

  it("keeps doc comments as field docs, and reads quoted keys and optional fields", () => {
    const t = parseTsType(`{ a: { x: number, }, \n/**\n * When it was checked off.\n */\n"b-c"?: string, }`);
    expect(fields(t)).toEqual([
      { name: "a", optional: false, type: { kind: "object", fields: [{ name: "x", optional: false, type: { kind: "number" } }] } },
      { name: "b-c", optional: true, type: { kind: "string" }, doc: "When it was checked off." },
    ]);
  });

  it("models named types as unknown, and gives null for text it can't read", () => {
    expect(fields(parseTsType(`{ address: Address, }`))![0]!.type).toEqual({ kind: "unknown", text: "Address" });
    expect(parseTsType(`{ a: string`)).toBeNull();
    expect(parseTsType(`{ a: % }`)).toBeNull();
    expect(parseTsType(`{ [key in string`)).toBeNull(); // an unclosed index signature ends, not loops
    expect(parseTsType(null)).toBeNull();
  });
});

describe("fieldKind", () => {
  const kind = (src: string) => fieldKind(fields(parseTsType(`{ f: ${src}, }`))![0]!.type);

  it("picks a widget and nullability", () => {
    expect(kind("string")).toEqual({ kind: { kind: "string" }, nullable: false });
    expect(kind("number | null")).toEqual({ kind: { kind: "number" }, nullable: true });
    expect(kind(`Id<"_users"> | null`)).toEqual({ kind: { kind: "id", table: "_users" }, nullable: true });
    expect(kind(`"a" | "b"`)).toEqual({ kind: { kind: "enum", options: ["a", "b"] }, nullable: false });
    expect(kind("Array<string>")).toEqual({ kind: { kind: "json" }, nullable: false });
    expect(kind("string | number")).toEqual({ kind: { kind: "json" }, nullable: false });
    expect(kind("{ x: number }").kind.kind).toBe("object");
  });

  it("infers a widget from a value", () => {
    expect(kindOfValue("x")).toEqual({ kind: "string" });
    expect(kindOfValue(1)).toEqual({ kind: "number" });
    expect(kindOfValue(true)).toEqual({ kind: "boolean" });
    expect(kindOfValue(null)).toEqual({ kind: "json" });
    expect(kindOfValue([1])).toEqual({ kind: "json" });
  });
});

describe("defaultObject", () => {
  it("fills required fields with blanks", () => {
    const t = parseTsType(`{ text: string, done: boolean, n: number, state: "a" | "b", image: Id<"_storage"> | null, opt?: string, nested: { x: number }, list: Array<string>, }`);
    expect(defaultObject(fields(t)!)).toEqual({ text: "", done: false, n: 0, state: "a", image: null, nested: { x: 0 }, list: [] });
  });
});
