/**
 * The split-wipe Compare of the preview: where the bar is and how it moves. The engine sends the camera picture and the
 * output in one frame while Compare is on (`FORMAT_NV12_PAIR`, `previewFrame.ts`); the page draws the camera picture
 * ("Before") on one side of the bar and the output ("After") on the other. Pure: no DOM, no store.
 *
 * `pos` is the bar's place along its axis as a fraction of the picture, 0 to 1. A vertical bar is dragged left and right:
 * Before is on its left, After on its right. A horizontal bar is dragged up and down: Before above, After below.
 */
import { selectedFilterId } from "./layout";
import { compareFilter, isCompare, letterbox, samePoint, type PreviewPoint, type Rect } from "./previewFrame";

export type Orientation = "vertical" | "horizontal";

/** The bar and the side it splits, as the renderer and the overlay read them. */
export interface Split {
  pos: number;
  orientation: Orientation;
}

export const DEFAULT_SPLIT: Split = { pos: 0.5, orientation: "vertical" };

/** What an arrow key moves the bar by, as a fraction of the picture (Shift: the large step). */
export const STEP = 0.02;
export const BIG_STEP = 0.1;

/** A label ("Before" / "After") is hidden when the side it names is thinner than this many pixels: it would sit on the bar or the chips. */
export const LABEL_PX = 72;
/** The widest a label grows (a long filter name is cut with an ellipsis, `.compare-label` in `styles.css`). */
export const LABEL_MAX_PX = 220;

/**
 * What Compare splits for a selection: the picture entering and leaving the selected filter (`{ compare: id }`), or, with
 * anything else selected (camera, controls, output) or a filter that is not in `chain`, the camera picture against the output.
 */
export function comparePoint(selected: string, chain: readonly { id: string }[]): PreviewPoint {
  const id = selectedFilterId(selected);
  return id !== null && chain.some((f) => f.id === id) ? { compare: id } : "compare";
}

/**
 * The point to switch to when the selection or the chain changed while `point` is showing: Compare follows the selection live
 * (`comparePoint`). `undefined` when `point` is no Compare, or already the right one.
 */
export function retargetCompare(point: PreviewPoint, selected: string, chain: readonly { id: string }[]): PreviewPoint | undefined {
  if (!isCompare(point)) return undefined;
  const next = comparePoint(selected, chain);
  return samePoint(point, next) ? undefined : next;
}

/** The text on the two sides: "Before" / "After", or for the Compare of a filter "Before <filter label>" / "After". */
export function compareLabels(point: PreviewPoint, steps: readonly { id: string; label: string }[]): { before: string; after: string } {
  const id = compareFilter(point);
  const step = id === null ? undefined : steps.find((s) => s.id === id);
  return { before: step ? `Before ${step.label}` : "Before", after: "After" };
}

/** Where a label sits from the picture's edge or the bar (`.compare-label` in `styles.css`). */
export const LABEL_OFFSET_PX = 8;

/** The room a label needs, in pixels along the bar's axis: `LABEL_PX` for a short one, more for a longer text, at most `LABEL_MAX_PX` plus its offset. */
export const labelPx = (text: string): number => Math.min(LABEL_MAX_PX + LABEL_OFFSET_PX, Math.max(LABEL_PX, 32 + text.length * 6));

export const swap = (o: Orientation): Orientation => (o === "vertical" ? "horizontal" : "vertical");

/** `pos` kept inside the picture and rounded to a tenth of a percent (so repeated steps do not drift). */
export function clampPos(pos: number): number {
  if (!Number.isFinite(pos)) return DEFAULT_SPLIT.pos;
  return Math.round(Math.min(Math.max(pos, 0), 1) * 1000) / 1000;
}

/**
 * Where the picture is inside the preview box, in box pixels: the box letterboxes the canvas (`object-fit: contain`), and the bar
 * lives on the picture, not on the box.
 */
export function pictureRect(box: { width: number; height: number }, canvas: { width: number; height: number }): Rect {
  if (box.width <= 0 || box.height <= 0 || canvas.width <= 0 || canvas.height <= 0) return { left: 0, top: 0, width: box.width, height: box.height };
  const { s, ox, oy } = letterbox(box, canvas);
  return { left: ox, top: oy, width: canvas.width * s, height: canvas.height * s };
}

/** The bar's position for a pointer at (`clientX`, `clientY`), given the picture's rect on the screen. Clamped to the picture. */
export function posFromPointer(clientX: number, clientY: number, rect: Rect, orientation: Orientation): number {
  const [at, from, size] = orientation === "vertical" ? [clientX, rect.left, rect.width] : [clientY, rect.top, rect.height];
  return size > 0 ? clampPos((at - from) / size) : DEFAULT_SPLIT.pos;
}

/**
 * The new position for a key on the focused bar, or `null` when the key is not one the bar uses. Left and Up move the bar toward
 * the start of its axis, Right and Down toward the end, whichever way it lies; Shift takes the large step; Home and End go to
 * the edges.
 */
export function posFromKey(pos: number, key: string, shift: boolean): number | null {
  const step = shift ? BIG_STEP : STEP;
  switch (key) {
    case "ArrowLeft":
    case "ArrowUp":
      return clampPos(pos - step);
    case "ArrowRight":
    case "ArrowDown":
      return clampPos(pos + step);
    case "Home":
      return 0;
    case "End":
      return 1;
    default:
      return null;
  }
}

/**
 * Whether the label of a side shows: `Before` is the side from 0 to `pos`, `After` the side from `pos` to 1, on a picture that is
 * `extent` pixels along the bar's axis; `need` is the room the text takes (`labelPx`).
 */
export function labelShown(side: "before" | "after", pos: number, extent: number, need = LABEL_PX): boolean {
  return (side === "before" ? pos : 1 - pos) * extent >= need;
}

/** A saved split read back: anything that is not a good position and orientation falls back to the default, field by field. */
export function readSaved(raw: string | null): Split {
  try {
    const v = raw === null ? null : (JSON.parse(raw) as Partial<Split> | null);
    return {
      pos: typeof v?.pos === "number" ? clampPos(v.pos) : DEFAULT_SPLIT.pos,
      orientation: v?.orientation === "horizontal" ? "horizontal" : "vertical",
    };
  } catch {
    return DEFAULT_SPLIT;
  }
}

/** The key that switches the bar's orientation (with the preview focused or the bar), named in the tooltip. */
export const ORIENT_KEY = "O";

/** Whether a key press is the orientation switch: the plain key, not a shortcut and not typing into a field. */
export function isOrientKey(e: { key: string; ctrlKey: boolean; metaKey: boolean; altKey: boolean }, typing: boolean): boolean {
  return !typing && !e.ctrlKey && !e.metaKey && !e.altKey && e.key.toLowerCase() === ORIENT_KEY.toLowerCase();
}
