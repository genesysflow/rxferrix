/**
 * The TS type of a table's documents (`TableSummary.ts`, as generated from its Rust type), parsed into a tree the
 * document form renders fields from. The parser is tolerant: a construct it doesn't know becomes `unknown` (edited
 * as JSON), and text it can't parse at all gives `null` (the form then goes by the document's values).
 */

import type { Json, JsonObject } from "./api.js";

export type TsType =
  | { kind: "string" }
  | { kind: "number" }
  | { kind: "boolean" }
  | { kind: "null" }
  | { kind: "any" }
  | { kind: "literal"; value: string | number | boolean }
  | { kind: "id"; table: string }
  | { kind: "array"; of: TsType }
  | { kind: "tuple"; of: TsType[] }
  | { kind: "record"; of: TsType }
  | { kind: "object"; fields: TsField[] }
  | { kind: "union"; of: TsType[] }
  /** A named type (not written out), or a construct the parser doesn't model. */
  | { kind: "unknown"; text: string };

export interface TsField {
  name: string;
  optional: boolean;
  type: TsType;
  /** The field's doc comment, if any. */
  doc?: string;
}

type Token = { t: "punct"; v: string } | { t: "ident"; v: string } | { t: "string"; v: string } | { t: "number"; v: number } | { t: "doc"; v: string };

function tokenize(src: string): Token[] {
  const out: Token[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i]!;
    if (/\s/.test(c)) {
      i++;
    } else if (src.startsWith("/**", i)) {
      const end = src.indexOf("*/", i + 3);
      if (end < 0) throw new Error("unterminated comment");
      const text = src
        .slice(i + 3, end)
        .split("\n")
        .map((l) => l.replace(/^\s*\*\s?/, "").trim())
        .filter(Boolean)
        .join(" ");
      out.push({ t: "doc", v: text });
      i = end + 2;
    } else if (src.startsWith("/*", i)) {
      const end = src.indexOf("*/", i + 2);
      if (end < 0) throw new Error("unterminated comment");
      i = end + 2;
    } else if (src.startsWith("//", i)) {
      const end = src.indexOf("\n", i);
      i = end < 0 ? src.length : end + 1;
    } else if (c === '"' || c === "'") {
      let j = i + 1;
      let s = "";
      while (j < src.length && src[j] !== c) {
        if (src[j] === "\\") j++;
        s += src[j] ?? "";
        j++;
      }
      if (j >= src.length) throw new Error("unterminated string");
      out.push({ t: "string", v: s });
      i = j + 1;
    } else if (/\d/.test(c) || (c === "-" && /\d/.test(src[i + 1] ?? ""))) {
      const m = /^-?\d+(\.\d+)?([eE][-+]?\d+)?/.exec(src.slice(i));
      if (!m) throw new Error(`unexpected ${c}`);
      out.push({ t: "number", v: Number(m[0]) });
      i += m[0].length;
    } else if (/[A-Za-z_$]/.test(c)) {
      const m = /^[A-Za-z_$][\w$]*/.exec(src.slice(i))!;
      out.push({ t: "ident", v: m[0] });
      i += m[0].length;
    } else if ("{}[]()<>|&,:;?=".includes(c)) {
      out.push({ t: "punct", v: c });
      i++;
    } else {
      throw new Error(`unexpected ${c}`);
    }
  }
  return out;
}

class Parser {
  private i = 0;
  constructor(
    private readonly tokens: Token[],
    private readonly src: string,
  ) {}

  private peek(): Token | undefined {
    return this.tokens[this.i];
  }
  private isPunct(v: string): boolean {
    const t = this.peek();
    return t?.t === "punct" && t.v === v;
  }
  private eat(v: string): boolean {
    if (!this.isPunct(v)) return false;
    this.i++;
    return true;
  }
  private expect(v: string) {
    if (!this.eat(v)) throw new Error(`expected ${v}`);
  }
  private skipDocs(): string | undefined {
    let doc: string | undefined;
    while (this.peek()?.t === "doc") doc = (this.tokens[this.i++] as { v: string }).v;
    return doc;
  }

  done(): boolean {
    this.skipDocs();
    return this.i >= this.tokens.length;
  }

  type(): TsType {
    this.skipDocs();
    this.eat("|");
    const parts = [this.postfix()];
    while (this.eat("|")) parts.push(this.postfix());
    if (this.isPunct("&")) {
      // Intersections: not modeled (edited as JSON).
      while (this.eat("&")) this.postfix();
      return { kind: "unknown", text: this.src };
    }
    const flat = parts.flatMap((p) => (p.kind === "union" ? p.of : [p]));
    return flat.length === 1 ? flat[0]! : { kind: "union", of: flat };
  }

  private postfix(): TsType {
    let t = this.primary();
    while (this.isPunct("[") && this.tokens[this.i + 1]?.t === "punct" && (this.tokens[this.i + 1] as { v: string }).v === "]") {
      this.i += 2;
      t = { kind: "array", of: t };
    }
    return t;
  }

  private primary(): TsType {
    this.skipDocs();
    const tok = this.tokens[this.i++];
    if (!tok) throw new Error("unexpected end");
    if (tok.t === "string") return { kind: "literal", value: tok.v };
    if (tok.t === "number") return { kind: "literal", value: tok.v };
    if (tok.t === "punct") {
      if (tok.v === "(") {
        const t = this.type();
        this.expect(")");
        return t;
      }
      if (tok.v === "{") return this.object();
      if (tok.v === "[") {
        const of: TsType[] = [];
        while (!this.eat("]")) {
          of.push(this.type());
          if (!this.eat(",")) {
            this.expect("]");
            break;
          }
        }
        return { kind: "tuple", of };
      }
      throw new Error(`unexpected ${tok.v}`);
    }
    if (tok.t !== "ident") throw new Error("unexpected comment");
    const args = this.isPunct("<") ? this.typeArgs() : [];
    switch (tok.v) {
      case "string":
        return { kind: "string" };
      case "number":
      case "bigint":
        return { kind: "number" };
      case "boolean":
        return { kind: "boolean" };
      case "true":
      case "false":
        return { kind: "literal", value: tok.v === "true" };
      case "null":
      case "undefined":
        return { kind: "null" };
      case "any":
      case "unknown":
        return { kind: "any" };
      case "Array":
        return { kind: "array", of: args[0] ?? { kind: "any" } };
      case "Record":
        return { kind: "record", of: args[1] ?? { kind: "any" } };
      case "Id": {
        const a = args[0];
        return a?.kind === "literal" && typeof a.value === "string" ? { kind: "id", table: a.value } : { kind: "string" };
      }
      default:
        return { kind: "unknown", text: tok.v };
    }
  }

  private typeArgs(): TsType[] {
    this.expect("<");
    const out = [this.type()];
    while (this.eat(",")) out.push(this.type());
    this.expect(">");
    return out;
  }

  private object(): TsType {
    const fields: TsField[] = [];
    let record: TsType | null = null;
    for (;;) {
      const doc = this.skipDocs();
      if (this.eat("}")) break;
      if (this.eat("[")) {
        // An index signature: `[key in string]?: T` or `[key: string]: T`.
        while (!this.eat("]")) {
          if (this.i >= this.tokens.length) throw new Error("expected ]");
          this.i++;
        }
        this.eat("?");
        this.expect(":");
        record = this.type();
      } else {
        const key = this.tokens[this.i++];
        if (!key || (key.t !== "ident" && key.t !== "string" && key.t !== "number")) throw new Error("expected a field name");
        const optional = this.eat("?");
        this.expect(":");
        fields.push({ name: String(key.v), optional, type: this.type(), ...(doc ? { doc } : {}) });
      }
      if (!this.eat(",") && !this.eat(";")) {
        this.skipDocs();
        this.expect("}");
        break;
      }
    }
    if (record && fields.length === 0) return { kind: "record", of: record };
    return { kind: "object", fields };
  }
}

/** Parses a TS type, or `null` if the text isn't one this parser reads. */
export function parseTsType(src: string | null | undefined): TsType | null {
  if (!src) return null;
  try {
    const p = new Parser(tokenize(src), src.trim());
    const t = p.type();
    return p.done() ? t : null;
  } catch {
    return null;
  }
}

/** How the form edits a value of a type. */
export type FieldKind =
  | { kind: "string" }
  | { kind: "number" }
  | { kind: "boolean" }
  | { kind: "id"; table: string }
  | { kind: "enum"; options: (string | number | boolean)[] }
  | { kind: "object"; fields: TsField[] }
  | { kind: "json" };

/** The widget for a type, and whether it also accepts `null`. */
export function fieldKind(type: TsType): { kind: FieldKind; nullable: boolean } {
  const parts = type.kind === "union" ? type.of : [type];
  const nullable = parts.some((p) => p.kind === "null");
  const rest = parts.filter((p) => p.kind !== "null");
  if (rest.length === 0) return { kind: { kind: "json" }, nullable };
  if (rest.every((p) => p.kind === "literal")) {
    return { kind: { kind: "enum", options: rest.map((p) => (p as { value: string | number | boolean }).value) }, nullable };
  }
  if (rest.length > 1) {
    // `true | false` style unions of booleans read as a boolean; anything else mixed is edited as JSON.
    if (rest.every((p) => p.kind === "boolean" || (p.kind === "literal" && typeof p.value === "boolean"))) return { kind: { kind: "boolean" }, nullable };
    return { kind: { kind: "json" }, nullable };
  }
  const t = rest[0]!;
  switch (t.kind) {
    case "string":
    case "number":
    case "boolean":
      return { kind: { kind: t.kind }, nullable };
    case "id":
      return { kind: { kind: "id", table: t.table }, nullable };
    case "object":
      return { kind: { kind: "object", fields: t.fields }, nullable };
    default:
      return { kind: { kind: "json" }, nullable };
  }
}

/** The widget for a value without a type (a table not defined in code, a field the type doesn't list). */
export function kindOfValue(value: Json | undefined): FieldKind {
  if (typeof value === "string") return { kind: "string" };
  if (typeof value === "number") return { kind: "number" };
  if (typeof value === "boolean") return { kind: "boolean" };
  return { kind: "json" };
}

/** A blank value of a type: what a new document starts with. */
export function defaultValue(type: TsType): Json {
  switch (type.kind) {
    case "string":
    case "id":
      return "";
    case "number":
      return 0;
    case "boolean":
      return false;
    case "literal":
      return type.value;
    case "array":
    case "tuple":
      return [];
    case "record":
      return {};
    case "object":
      return defaultObject(type.fields);
    case "union": {
      if (type.of.some((p) => p.kind === "null")) return null;
      return defaultValue(type.of[0]!);
    }
    default:
      return null;
  }
}

/** A blank document of an object type: every required field at its default (optional fields left out). */
export function defaultObject(fields: TsField[]): JsonObject {
  const out: JsonObject = {};
  for (const f of fields) if (!f.optional) out[f.name] = defaultValue(f.type);
  return out;
}

/** A short label for a type, shown beside a field (`string | null`, `Id<"_storage">`, `"a" | "b"`). */
export function typeText(type: TsType): string {
  switch (type.kind) {
    case "string":
    case "number":
    case "boolean":
    case "null":
    case "any":
      return type.kind;
    case "literal":
      return JSON.stringify(type.value);
    case "id":
      return `Id<"${type.table}">`;
    case "array":
      return `${typeText(type.of)}[]`;
    case "tuple":
      return `[${type.of.map(typeText).join(", ")}]`;
    case "record":
      return `{ [key]: ${typeText(type.of)} }`;
    case "object":
      return "object";
    case "union":
      return type.of.map(typeText).join(" | ");
    case "unknown":
      return type.text.length > 40 ? "…" : type.text;
  }
}
