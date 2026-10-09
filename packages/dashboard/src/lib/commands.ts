/**
 * The command palette's matching (unit-tested in test/commands.test.ts): every query character must appear in order
 * in the label (or a keyword); contiguous runs, word starts and earlier matches rank higher.
 */

export interface Command {
  id: string;
  label: string;
  /** Shown on the right, e.g. "Page", "Table". */
  group: string;
  /** More words that match (not shown). */
  keywords?: string;
  /** A shortcut to show, e.g. "Alt+2". */
  shortcut?: string;
  run(): void;
}

/** The match score of `query` in `text` (higher is better), or null when it doesn't match. */
export function fuzzyScore(query: string, text: string): number | null {
  const q = query.toLowerCase().replace(/\s+/g, "");
  if (!q) return 0;
  const t = text.toLowerCase();
  let score = 0;
  let from = 0;
  let previous = -2;
  for (const ch of q) {
    const i = t.indexOf(ch, from);
    if (i === -1) return null;
    const wordStart = i === 0 || /[\s:/_\-.]/.test(t[i - 1]!);
    score += 1 + (i === previous + 1 ? 3 : 0) + (wordStart ? 2 : 0) - Math.min(i - from, 5) * 0.1;
    previous = i;
    from = i + 1;
  }
  // Prefer shorter texts (a closer match) and matches at the start.
  return score - t.length * 0.01 + (t.startsWith(q) ? 5 : 0);
}

/** The commands matching `query`, best first (all of them, in order, for an empty query). */
export function rankCommands<C extends Command>(query: string, commands: readonly C[], limit = 50): C[] {
  if (!query.trim()) return commands.slice(0, limit);
  const scored: { c: C; s: number; i: number }[] = [];
  commands.forEach((c, i) => {
    const label = fuzzyScore(query, c.label);
    const keywords = c.keywords ? fuzzyScore(query, c.keywords) : null;
    const s = Math.max(label ?? -Infinity, keywords === null ? -Infinity : keywords - 1);
    if (s > -Infinity) scored.push({ c, s, i });
  });
  scored.sort((a, b) => b.s - a.s || a.i - b.i);
  return scored.slice(0, limit).map((x) => x.c);
}
