/**
 * Windowing math for the virtualized document grid (unit-tested in test/virtual.test.ts): only the rows in view
 * (plus some overscan) are rendered; spacer rows above and below keep the scroll height right. Rows have a fixed
 * height, so everything is arithmetic on `scrollTop`.
 */

export interface WindowInput {
  /** The scroll container's `scrollTop`, in px, measured from the top of the first row. */
  scrollTop: number;
  /** The visible height of the scroll container (minus the header), in px. */
  viewportHeight: number;
  rowHeight: number;
  /** Rows in total (loaded). */
  count: number;
  /** Extra rows rendered above and below the visible ones. */
  overscan?: number;
}

export interface RowWindow {
  /** First rendered row (inclusive). */
  start: number;
  /** Last rendered row (exclusive). */
  end: number;
  /** Height of the spacer above the first rendered row. */
  padTop: number;
  /** Height of the spacer below the last rendered row. */
  padBottom: number;
}

export function rowWindow({ scrollTop, viewportHeight, rowHeight, count, overscan = 8 }: WindowInput): RowWindow {
  if (count <= 0 || rowHeight <= 0) return { start: 0, end: 0, padTop: 0, padBottom: 0 };
  const top = Math.max(0, Math.min(scrollTop, count * rowHeight));
  const first = Math.floor(top / rowHeight);
  const visible = Math.ceil(Math.max(viewportHeight, 0) / rowHeight) + 1;
  const start = Math.max(0, first - overscan);
  const end = Math.min(count, first + visible + overscan);
  return { start, end, padTop: start * rowHeight, padBottom: (count - end) * rowHeight };
}

/**
 * Whether to load the next page: the rendered window reaches within `threshold` rows of the end of what is loaded.
 */
export function nearEnd(window: RowWindow, count: number, threshold = 50): boolean {
  return count === 0 || window.end >= count - threshold;
}

/** The `scrollTop` that brings row `index` into view (unchanged if it already is). */
export function scrollToRow(index: number, scrollTop: number, viewportHeight: number, rowHeight: number): number {
  const top = index * rowHeight;
  const bottom = top + rowHeight;
  if (top < scrollTop) return top;
  if (bottom > scrollTop + viewportHeight) return bottom - viewportHeight;
  return scrollTop;
}

/**
 * Column widths (px) for a fixed-layout grid, from the longest rendered text in a sample of rows: stable while
 * scrolling (rows come and go, widths don't jump), clamped to `[min, max]`.
 */
export function columnWidths(
  columns: readonly string[],
  sample: readonly Record<string, unknown>[],
  { charWidth = 7.8, padding = 24, min = 72, max = 320 } = {},
): number[] {
  return columns.map((c) => {
    let longest = c.length;
    for (const row of sample) {
      const v = row[c];
      const len =
        v === undefined ? 1 : c === "_creationTime" ? 19 : typeof v === "string" ? v.length : JSON.stringify(v).length;
      if (len > longest) longest = len;
    }
    return Math.round(Math.min(max, Math.max(min, longest * charWidth + padding)));
  });
}
