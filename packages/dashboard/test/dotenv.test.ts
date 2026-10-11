import { describe, expect, it } from "vitest";
import { parseDotenv } from "../src/lib/dotenv.js";

describe("parseDotenv", () => {
  it("reads NAME=value lines, quotes, comments and export", () => {
    const text = [
      "# a comment",
      "",
      "API_URL=https://api.example.com",
      "export TOKEN = abc123   # trailing comment",
      `GREETING="Hello, \\"world\\"\\nbye"`,
      "RAW='no $escapes \\n here'",
      "EMPTY=",
      "API_URL=https://override.example.com",
    ].join("\n");
    expect(parseDotenv(text)).toEqual({
      entries: [
        ["API_URL", "https://override.example.com"],
        ["TOKEN", "abc123"],
        ["GREETING", 'Hello, "world"\nbye'],
        ["RAW", "no $escapes \\n here"],
        ["EMPTY", ""],
      ],
      errors: [],
    });
  });

  it("reports lines it can't use", () => {
    const { entries, errors } = parseDotenv("just text\n1BAD=x\nOK=1\nQ=\"open");
    expect(entries).toEqual([["OK", "1"]]);
    expect(errors).toEqual(["line 1: expected NAME=value", "line 2: “1BAD” is not a valid name", 'line 4: unterminated "']);
  });
});
