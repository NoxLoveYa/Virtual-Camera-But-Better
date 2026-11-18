/**
 * Pure rules of "insert a filter at a position" (L5): what a press on an Add-menu item turns into, whether the pointer is
 * over the chain, and the words of the "+" between two cards. No DOM and no React here, so `npm run test:ui` can run it
 * under node. The pointer session that uses it is `insertDragSession.ts`.
 */
import { DRAG_THRESHOLD } from "./chainDrag";

/** How far above and below the chain block (in px) a pointer still counts as "over the chain": the drop clamps to the first or last gap. */
export const CHAIN_MARGIN = 40;

export type Press = "wait" | "drag" | "scroll";

/**
 * What a press on an Add-menu item has become after the pointer moved (`dx`, `dy`) from where it went down.
 * A mouse or a pen drags in any direction. A finger that moves mostly sideways drags (the menu is a vertical list, so a
 * vertical move is the list scrolling and the press gives up).
 */
export function classifyPress(pointerType: string, dx: number, dy: number): Press {
  if (Math.hypot(dx, dy) < DRAG_THRESHOLD) return "wait";
  if (pointerType === "touch" && Math.abs(dy) >= Math.abs(dx)) return "scroll";
  return "drag";
}

interface Box {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

/**
 * Whether a point is over the chain: between the chain block's left and right edges, and between its top and bottom edges
 * widened by `CHAIN_MARGIN`, but only inside what the scrolling column shows. Outside, a drop adds nothing.
 */
export function overChain(x: number, y: number, block: Box, scroller: Box, margin = CHAIN_MARGIN): boolean {
  if (x < block.left || x > block.right) return false;
  const top = Math.max(scroller.top, block.top - margin);
  const bottom = Math.min(scroller.bottom, block.bottom + margin);
  return y >= top && y <= bottom;
}

/** The accessible name of the "+" in gap `k` (between step `k` and step `k + 1`, 1-based), as in "Add a filter between step 2 and 3". */
export function gapLabel(k: number): string {
  return `Add a filter between step ${k} and ${k + 1}`;
}
