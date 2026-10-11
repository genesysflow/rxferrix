/**
 * Animations for live changes in tables (the data grid, the logs): a row that appeared slides in with a tint, a
 * changed cell flashes, a removed row fades out and closes up. Used as a Svelte action, `use:flash={change}`; each
 * change plays once (keyed by its `at`), partway through if the element was created after it started (a row
 * scrolled into view), and not at all once it is over.
 */

export const FLASH_MS = 1800;
export const GHOST_MS = 700;

export interface Flash {
  kind: "inserted" | "changed" | "removed";
  /** When the change was seen (`performance.now()`). */
  at: number;
  /** The theme color to tint with (default: `--ok` for inserted, `--warn` for changed, `--err` for removed). */
  color?: string;
  /** Row height in px, for a removed row's collapse. */
  rowHeight?: number;
}

const reducedMotion = () => typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;

/** A theme color (`--ok`, ...) at `alpha`. */
function tint(node: HTMLElement, name: string, alpha: number): string {
  const hex = getComputedStyle(node).getPropertyValue(name).trim();
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex);
  return m ? `rgb(${parseInt(m[1]!, 16)} ${parseInt(m[2]!, 16)} ${parseInt(m[3]!, 16)} / ${alpha})` : hex;
}

function keyframes(node: HTMLElement, f: Flash, still: boolean): Keyframe[] {
  if (f.kind === "inserted") {
    const c = tint(node, f.color ?? "--ok", 0.22);
    return still
      ? [{ backgroundColor: c }, { backgroundColor: c, offset: 0.4 }]
      : [
          { backgroundColor: c, opacity: 0, transform: "translateY(-10px)" },
          { backgroundColor: c, opacity: 1, transform: "none", offset: 0.12 },
          { backgroundColor: c, offset: 0.4 },
        ];
  }
  if (f.kind === "changed") {
    const c = tint(node, f.color ?? "--warn", 0.3);
    return [{ backgroundColor: c }, { backgroundColor: c, offset: 0.35 }];
  }
  const c = tint(node, f.color ?? "--err", 0.16);
  return [
    { backgroundColor: c, opacity: 1 },
    { backgroundColor: c, opacity: 0 },
  ];
}

export function flash(node: HTMLElement, change: Flash | undefined) {
  let played = 0;
  const play = (f: Flash | undefined) => {
    if (!f || f.at === played || typeof node.animate !== "function") return;
    played = f.at;
    const duration = f.kind === "removed" ? GHOST_MS : FLASH_MS;
    const age = performance.now() - f.at;
    if (age >= duration) return;
    const still = reducedMotion();
    const timing: KeyframeAnimationOptions = { duration, delay: -age, easing: "ease-out", fill: f.kind === "removed" ? "forwards" : "none" };
    node.animate(keyframes(node, f, still), timing);
    // A removed row then closes up: its cells shrink to nothing, so the rows below slide up instead of jumping.
    if (f.kind === "removed" && !still) {
      const h = f.rowHeight ?? 32;
      for (const cell of node.children) {
        (cell as HTMLElement).animate(
          [
            { height: `${h}px`, lineHeight: `${h - 1}px`, offset: 0.55 },
            { height: "0px", lineHeight: "0px", fontSize: "0px", borderBottomWidth: "0px" },
          ],
          { ...timing, easing: "ease-in-out" },
        );
      }
    }
  };
  play(change);
  return { update: play };
}
