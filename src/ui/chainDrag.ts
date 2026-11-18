/**
 * Pure logic of the chain reorder (W1): where a card lands, how fast the column scrolls near its edge, and what the
 * keyboard does to the drop line. No DOM and no React here, so `npm run test:ui` can run it under node. The hook that
 * drives it from pointer and key events is `useChainDrag.ts`.
 *
 * A "gap" k in 0…n is the place above card k (k = n: under the last card). Dropping the card at index `from` into gap k
 * leaves it at index `finalIndex(from, k)`; the gaps `from` and `from + 1` are both "where it already is".
 */

/** Pixels from the scroll container's top or bottom edge where auto-scroll starts. */
export const EDGE_ZONE = 56;
/** Scroll speed at (or beyond) the edge, in pixels per animation frame. */
export const EDGE_MAX_SPEED = 18;
/** Movement before a press becomes a drag, so a click is still a click. */
export const DRAG_THRESHOLD = 4;

/** The gap the pointer is in: the number of card midpoints above `y`. A pointer exactly on a midpoint is above it. */
export function insertionIndex(midpoints: readonly number[], y: number): number {
  let k = 0;
  for (const m of midpoints) if (y > m) k++;
  return k;
}

/** Where the dragged card ends up when dropped into gap `k`. */
export function finalIndex(from: number, k: number): number {
  return k > from ? k - 1 : k;
}

/** The gap that puts the card at index `final`. The inverse of `finalIndex`. */
export function gapFor(from: number, final: number): number {
  return final > from ? final + 1 : final;
}

/** The two gaps next to the source card both mean "stay": the line is shown at the source slot and a drop does nothing. */
export function snapToSource(from: number, k: number): number {
  return k === from || k === from + 1 ? from : k;
}

/**
 * Scroll step for this frame: negative near the top edge, positive near the bottom, 0 elsewhere. It grows linearly from
 * 0 at the zone's inner border to `max` at the edge, and stays at `max` beyond it.
 */
export function edgeSpeed(y: number, top: number, bottom: number, zone = EDGE_ZONE, max = EDGE_MAX_SPEED): number {
  if (y < top + zone) return -max * Math.min(1, (top + zone - y) / zone);
  if (y > bottom - zone) return max * Math.min(1, (y - (bottom - zone)) / zone);
  return 0;
}

export type StepKey = "ArrowUp" | "ArrowDown" | "Home" | "End";

/** The gap after a navigation key while a card is lifted from the keyboard: it lands one step earlier or later, or at an end. */
export function keyboardGap(from: number, k: number, key: StepKey, count: number): number {
  const now = finalIndex(from, k);
  const next = key === "ArrowUp" ? now - 1 : key === "ArrowDown" ? now + 1 : key === "Home" ? 0 : count - 1;
  return gapFor(from, Math.max(0, Math.min(count - 1, next)));
}
