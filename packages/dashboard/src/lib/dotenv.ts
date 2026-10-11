/**
 * Parses pasted `.env` text for the Environment page (unit-tested in test/dotenv.test.ts): `NAME=value` lines,
 * optionally prefixed with `export`; `#` comments and blank lines skipped; values in single quotes taken as is, in
 * double quotes with `\n`, `\t`, `\"` and `\\` escapes; unquoted values trimmed, with a ` #` comment cut off.
 */

import { ENV_NAME } from "./api.js";

export interface Parsed {
  entries: [string, string][];
  /** One message per line that could not be used. */
  errors: string[];
}

export function parseDotenv(text: string): Parsed {
  const entries = new Map<string, string>();
  const errors: string[] = [];
  text.split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) return;
    const m = /^(?:export\s+)?([^=\s]+)\s*=\s*(.*)$/.exec(line);
    if (!m) {
      errors.push(`line ${i + 1}: expected NAME=value`);
      return;
    }
    const [, name, rest] = m as unknown as [string, string, string];
    if (!ENV_NAME.test(name)) {
      errors.push(`line ${i + 1}: “${name}” is not a valid name`);
      return;
    }
    let value: string;
    if (rest.startsWith('"')) {
      const end = closingQuote(rest);
      if (end < 0) {
        errors.push(`line ${i + 1}: unterminated "`);
        return;
      }
      value = rest
        .slice(1, end)
        .replace(/\\(.)/g, (_, c: string) => ({ n: "\n", t: "\t", r: "\r" })[c as "n" | "t" | "r"] ?? c);
    } else if (rest.startsWith("'")) {
      const end = rest.indexOf("'", 1);
      if (end < 0) {
        errors.push(`line ${i + 1}: unterminated '`);
        return;
      }
      value = rest.slice(1, end);
    } else {
      value = rest.replace(/\s+#.*$/, "").trim();
    }
    entries.set(name, value);
  });
  return { entries: [...entries], errors };
}

/** The index of the `"` closing the string that starts at 0, skipping escaped ones; -1 if none. */
function closingQuote(s: string): number {
  for (let i = 1; i < s.length; i++) {
    if (s[i] === "\\") i++;
    else if (s[i] === '"') return i;
  }
  return -1;
}
