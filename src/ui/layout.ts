/**
 * Pure logic of the window layout: the three docked regions (navigator | preview | inspector), their widths, the keys
 * of the two splitters, the saved state (`Saved.ui`) and the selection rule. No DOM and no store here, so
 * `layout.test.ts` can run it in node.
 */

/** Width of the 1 px hairline between two regions. */
export const HAIRLINE = 1;
/** The navigator's drag range, and its auto width `clamp(200px, 17vw, 260px)`. */
export const NAV_MIN = 184;
export const NAV_MAX = 320;
export const NAV_AUTO_MIN = 200;
export const NAV_AUTO_MAX = 260;
export const NAV_AUTO_VW = 0.17;
/** The inspector's drag range, and its auto width `clamp(340px, 25vw, 460px)`. */
export const INSPECTOR_MIN = 300;
export const INSPECTOR_MAX = 560;
export const INSPECTOR_AUTO_MIN = 340;
export const INSPECTOR_AUTO_MAX = 460;
export const INSPECTOR_AUTO_VW = 0.25;
/** The centre (the preview) never gets narrower than this: the navigator and the inspector give way first. */
export const CENTER_MIN = 360;
/** Below this window width the navigator is the icon rail. */
export const RAIL_BELOW = 1100;
/** The icon rail's width, and the full navigator's width as a flyout over the preview. */
export const RAIL_WIDTH = 52;
export const FLYOUT_WIDTH = 232;
/** Arrow keys on a splitter: 16 px, 64 px with Shift. */
export const KEY_STEP = 16;
export const KEY_STEP_BIG = 64;

/** What `Saved.ui` holds. */
export interface UiState {
  /** Navigator width in px; null = auto (clamp(200, 17vw, 260)). */
  navWidth: number | null;
  /** Inspector width in px; null = auto (clamp(340, 25vw, 460)). */
  inspectorWidth: number | null;
  /** The user collapsed the navigator to the rail (below 1100 px it is the rail anyway). */
  navRail: boolean;
  /** "camera" | "controls" | "output" | "filter:<id>". */
  selected: string;
  /** The Advanced group of the Camera inspector is open. */
  advanced: boolean;
}

export const DEFAULT_UI: UiState = { navWidth: null, inspectorWidth: null, navRail: false, selected: "camera", advanced: false };

const SECTIONS = ["camera", "controls", "output"] as const;
const FILTER_PREFIX = "filter:";

/** The selection string of a filter. */
export const filterSelection = (id: string) => `${FILTER_PREFIX}${id}`;
/** The filter id a selection names, or null for a section. */
export const selectedFilterId = (selected: string): string | null => (selected.startsWith(FILTER_PREFIX) ? selected.slice(FILTER_PREFIX.length) : null);

const isSelection = (v: unknown): v is string =>
  typeof v === "string" && ((SECTIONS as readonly string[]).includes(v) || (v.startsWith(FILTER_PREFIX) && v.length > FILTER_PREFIX.length));

const width = (v: unknown, min: number, max: number): number | null =>
  typeof v === "number" && Number.isFinite(v) && v >= min && v <= max ? Math.round(v) : null;

/**
 * The saved value, repaired: a width outside its drag range or a non-number is `null` (auto), a selection that is not
 * one of the four shapes is "camera", flags default to the table above. The old `split`, `camera`, `controls` and
 * `output` keys are dropped.
 */
export function normalizeUi(raw: unknown): UiState {
  const r = raw && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
  return {
    navWidth: width(r.navWidth, NAV_MIN, NAV_MAX),
    inspectorWidth: width(r.inspectorWidth, INSPECTOR_MIN, INSPECTOR_MAX),
    navRail: r.navRail === true,
    selected: isSelection(r.selected) ? r.selected : DEFAULT_UI.selected,
    advanced: r.advanced === true,
  };
}

const clamp = (v: number, min: number, max: number) => Math.min(max, Math.max(min, v));

/** `clamp(200px, 17vw, 260px)`. `total` is the workspace width, which is the window width. */
export const autoNavWidth = (total: number) => Math.round(clamp(total * NAV_AUTO_VW, NAV_AUTO_MIN, NAV_AUTO_MAX));
/** `clamp(340px, 25vw, 460px)`. */
export const autoInspectorWidth = (total: number) => Math.round(clamp(total * INSPECTOR_AUTO_VW, INSPECTOR_AUTO_MIN, INSPECTOR_AUTO_MAX));

/** What the workspace width minus its two hairlines leaves for the three regions. */
const roomOf = (total: number) => total - 2 * HAIRLINE;

/** The room the navigator may take: up to its own maximum, leaving the inspector at least its minimum and the centre its own. */
const navCeiling = (total: number) => Math.max(NAV_MIN, Math.min(NAV_MAX, roomOf(total) - INSPECTOR_MIN - CENTER_MIN));

export interface Widths {
  nav: number;
  inspector: number;
  /** What is left for the preview. */
  center: number;
  /** The navigator is the icon rail. */
  rail: boolean;
}

/** Whether the navigator is the icon rail in a workspace `total` px wide: below 1100 px, or when the user collapsed it. */
export const isRail = (total: number, navRail: boolean) => navRail || total < RAIL_BELOW;

/**
 * The widths the three regions have in a workspace `total` px wide: the navigator is the 52 px rail below 1100 px (or when
 * `navRail` is set), else its saved or auto width clamped to [184, room − 300 − 360]; the inspector is its saved or auto
 * width clamped to [300, min(560, room − nav − 360)]. The saved values are never changed by a narrow window, so widening
 * the window restores them.
 */
export function effectiveWidths(total: number, navSaved: number | null, inspectorSaved: number | null, navRail = false): Widths {
  const rail = isRail(total, navRail);
  const nav = rail ? RAIL_WIDTH : clamp(navSaved ?? autoNavWidth(total), NAV_MIN, navCeiling(total));
  const inspectorMax = Math.max(INSPECTOR_MIN, Math.min(INSPECTOR_MAX, roomOf(total) - nav - CENTER_MIN));
  const inspector = clamp(inspectorSaved ?? autoInspectorWidth(total), INSPECTOR_MIN, inspectorMax);
  return { nav, inspector, center: roomOf(total) - nav - inspector, rail };
}

export type Side = "nav" | "inspector";

/**
 * The widths a splitter may take, given the other region's current width. Dragging the navigator leaves the inspector
 * as it is, and the other way round.
 */
export function widthRange(side: Side, total: number, other: number): { min: number; max: number } {
  if (side === "nav") return { min: NAV_MIN, max: Math.max(NAV_MIN, Math.min(NAV_MAX, roomOf(total) - other - CENTER_MIN)) };
  return { min: INSPECTOR_MIN, max: Math.max(INSPECTOR_MIN, Math.min(INSPECTOR_MAX, roomOf(total) - other - CENTER_MIN)) };
}

/** The width after dragging `dx` px to the right from `start` (the inspector's left edge: to the right is narrower). */
export function widthFromDrag(side: Side, start: number, dx: number, range: { min: number; max: number }): number {
  return Math.round(clamp(side === "nav" ? start + dx : start - dx, range.min, range.max));
}

/**
 * What a key does to a splitter: the new width, `null` for "back to auto" (Enter), `undefined` for a key it does not
 * handle. The arrows move the line the way they point, so for the inspector ArrowLeft widens it. Home and End go to the
 * left-most and right-most line, which are the smallest and the largest navigator, the largest and the smallest inspector.
 */
export function widthAfterKey(side: Side, key: string, shift: boolean, current: number, range: { min: number; max: number }): number | null | undefined {
  const step = shift ? KEY_STEP_BIG : KEY_STEP;
  const towards = (dir: 1 | -1) => clamp(current + (side === "nav" ? dir : -dir) * step, range.min, range.max);
  switch (key) {
    case "ArrowLeft":
      return towards(-1);
    case "ArrowRight":
      return towards(1);
    case "Home":
      return side === "nav" ? range.min : range.max;
    case "End":
      return side === "nav" ? range.max : range.min;
    case "Enter":
      return null;
    default:
      return undefined;
  }
}

/**
 * The selection after the chain changed from `prev` to `next`: unchanged while the selected filter is still there (or a
 * section is selected); else the filter that took its place (the next one, else the previous one), else "output" (the
 * row above FILTERS). A selected filter that was not in `prev` either is "camera".
 */
export function selectionAfter(prev: readonly { id: string }[], next: readonly { id: string }[], selected: string): string {
  const id = selectedFilterId(selected);
  if (id === null || next.some((f) => f.id === id)) return selected;
  const i = prev.findIndex((f) => f.id === id);
  if (i < 0) return DEFAULT_UI.selected;
  if (next.length === 0) return "output";
  return filterSelection(next[Math.min(i, next.length - 1)].id);
}
