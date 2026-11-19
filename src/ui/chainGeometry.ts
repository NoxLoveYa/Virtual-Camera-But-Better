/**
 * The one place that knows where the chain's rows are on the page. Reorder (`useChainDrag.ts`), insert-drag
 * (`insertDragSession.ts`) and file drops (`useFileDrop.ts`) all need "which gap is under this y, and where does the drop
 * line go", and they all ask here.
 *
 * DOM contract (the navigator): the filter block carries `data-drop="chain"`; inside it the list is `ol.chain`; each row is
 * `li.nav-step[data-filter-id]`; the empty state is `li.empty`. A gap k in 0…n is the place above row k (k = n: under the
 * last row), as in `chainDrag.ts`.
 */
import { insertionIndex } from "./chainDrag";

/** The filter rows of the navigator list, top to bottom. */
export function rowRects(block: Element): DOMRect[] {
  return Array.from(block.querySelectorAll<HTMLElement>("ol.chain > li[data-filter-id]")).map((li) => li.getBoundingClientRect());
}

/**
 * Pure: the drop line's top for gap `k`, relative to the block. The line goes in the middle of the gap: between the rows
 * above and below it, or between the list's padded top (bottom) and the first (last) row, which is where the padding
 * holds it. With no row it goes at the list's top.
 */
export function gapLine(
  rows: readonly { top: number; bottom: number }[],
  k: number,
  listTop: number,
  listBottom: number,
  blockTop: number,
): number {
  const above = k <= 0 || rows.length === 0 ? listTop : rows[Math.min(k, rows.length) - 1].bottom;
  const below = k >= rows.length ? (rows.length === 0 ? listTop : listBottom) : rows[k].top;
  return Math.round((above + below) / 2 - blockTop - 1);
}

/**
 * The gap under client `y` and where its line goes (relative to the block). An empty chain has gap 0, with the line at the
 * top of `li.empty`, not through its text. `null` without a list.
 */
export function gapAt(block: Element, y: number): { gap: number; lineTop: number } | null {
  const ol = block.querySelector("ol.chain");
  if (!ol) return null;
  const blockTop = block.getBoundingClientRect().top;
  const rows = rowRects(block);
  if (rows.length === 0) {
    const empty = ol.querySelector("li.empty");
    return empty ? { gap: 0, lineTop: Math.round(empty.getBoundingClientRect().top - blockTop - 1) } : null;
  }
  const gap = insertionIndex(rows.map((r) => r.top + r.height / 2), y);
  const list = ol.getBoundingClientRect();
  return { gap, lineTop: gapLine(rows, gap, list.top, list.bottom, blockTop) };
}
