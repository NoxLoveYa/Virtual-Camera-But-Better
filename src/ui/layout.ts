/**
 * Pure logic of the window layout (W3): the column split ratio and the saved fold/split state (`Saved.ui`).
 * No DOM and no store here, so `layout.test.ts` can run it in node.
 */

/** The left column's share of the body today (6fr : 7fr). */
export const DEFAULT_SPLIT = 6 / 13;
/** The left column never gets narrower than this (the camera panel's two-column fields), nor the right one (a card's two parameter columns plus the rail). */
export const MIN_LEFT = 440;
export const MIN_RIGHT = 420;
/** Arrow keys on the splitter: 2 % of the body, 10 % with Shift. */
export const KEY_STEP = 0.02;
export const KEY_STEP_BIG = 0.1;

/** What `Saved.ui` holds: the split ratio (left width / body width) and which panels are open. */
export interface UiState {
  split: number;
  camera: boolean;
  controls: boolean;
  output: boolean;
  advanced: boolean;
}

export const DEFAULT_UI: UiState = { split: DEFAULT_SPLIT, camera: true, controls: true, output: true, advanced: false };

/** The saved value, repaired: a number outside (0.05, 0.95) or a non-number is the default; flags default to the table above. */
export function normalizeUi(raw: unknown): UiState {
  const r = raw && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
  const split = typeof r.split === "number" && Number.isFinite(r.split) && r.split > 0.05 && r.split < 0.95 ? r.split : DEFAULT_SPLIT;
  return {
    split,
    camera: r.camera !== false,
    controls: r.controls !== false,
    output: r.output !== false,
    advanced: r.advanced === true,
  };
}

/** The ratios the splitter may take in a body `total` px wide. A body too narrow for both minimums has one ratio: the default. */
export function splitBounds(total: number): { min: number; max: number } {
  if (!(total > 0)) return { min: DEFAULT_SPLIT, max: DEFAULT_SPLIT };
  const min = MIN_LEFT / total;
  const max = 1 - MIN_RIGHT / total;
  return min > max ? { min: DEFAULT_SPLIT, max: DEFAULT_SPLIT } : { min, max };
}

export function clampSplit(ratio: number, total: number): number {
  const { min, max } = splitBounds(total);
  return Math.min(max, Math.max(min, ratio));
}

/** The ratio for a left column `left` px wide. */
export const splitFromWidth = (left: number, total: number): number => clampSplit(left / total, total);

/** The ratio after a key press on the splitter, or null for a key it does not handle. */
export function splitAfterKey(key: string, shift: boolean, current: number, total: number): number | null {
  const step = shift ? KEY_STEP_BIG : KEY_STEP;
  switch (key) {
    case "ArrowLeft":
      return clampSplit(current - step, total);
    case "ArrowRight":
      return clampSplit(current + step, total);
    case "Home":
      return splitBounds(total).min;
    case "End":
      return splitBounds(total).max;
    case "Enter":
      return clampSplit(DEFAULT_SPLIT, total);
    default:
      return null;
  }
}
