/**
 * The maths of "Edit on picture" for Crop/Pad (UX-PASS L3): the window of a crop as four edges on the uncropped picture, what a
 * handle, the inside, the wheel or an arrow key does to it, and the params that come back. All sizes are source pixels (the full
 * size of the picture entering the filter), not preview pixels. Pure: no DOM, no store.
 *
 * Edges are positions on the picture: `l` and `t` the left and top edge, `r` and `b` the right and bottom edge (`r - l` is the
 * window's width). An edge outside `[0, size]` is padding (a negative Crop/Pad value). A gesture never writes padding: it works on
 * the window as it shows (an edge in padding is drawn at the picture edge) and writes only the edges it moved. So a padded edge
 * keeps its value until it is dragged, and a pan or a zoom, which moves every edge, writes them all inside the picture.
 */

export interface Size {
  w: number;
  h: number;
}

export interface Edges {
  l: number;
  t: number;
  r: number;
  b: number;
}

/** The params of a `crop_filter` as the engine reads them (`fx/geometry/crop.rs`). */
export interface CropParams {
  relative: boolean;
  left: number;
  top: number;
  right: number;
  bottom: number;
  cx: number;
  cy: number;
}

export type Handle = "nw" | "n" | "ne" | "e" | "se" | "s" | "sw" | "w";

/** The smallest window a gesture leaves, in source pixels (less when the picture itself is smaller). */
export const MIN_SIDE = 16;

/** Nudge by an arrow key: 1 source pixel, 10 with Shift. */
export const NUDGE = 1;
export const NUDGE_FAST = 10;

const int = (v: unknown, fallback: number) => (typeof v === "number" && Number.isFinite(v) ? Math.round(v) : fallback);
const clamp = (v: number, lo: number, hi: number) => Math.min(Math.max(v, lo), Math.max(lo, hi));

/** The crop params of a card (a missing or odd value is the default, as the engine reads it). */
export function readCrop(params: Record<string, unknown>): CropParams {
  return {
    relative: params.relative !== false,
    left: int(params.left, 0),
    top: int(params.top, 0),
    right: int(params.right, 0),
    bottom: int(params.bottom, 0),
    cx: Math.max(0, int(params.cx, 0)),
    cy: Math.max(0, int(params.cy, 0)),
  };
}

/** The window the params cut out of a picture of `src`. Relative: the four edges. Absolute: a window at (left, top); a size of 0 runs to the edge. */
export function edgesOf(p: CropParams, src: Size): Edges {
  if (p.relative) return { l: p.left, t: p.top, r: src.w - p.right, b: src.h - p.bottom };
  return { l: p.left, t: p.top, r: p.left + (p.cx === 0 ? src.w - p.left : p.cx), b: p.top + (p.cy === 0 ? src.h - p.top : p.cy) };
}

/** The params that cut out `e`, in the mode of `relative`. Whole pixels. */
export function paramsOf(e: Edges, relative: boolean, src: Size): Partial<CropParams> {
  const l = Math.round(e.l);
  const t = Math.round(e.t);
  const r = Math.round(e.r);
  const b = Math.round(e.b);
  return relative ? { left: l, top: t, right: src.w - r, bottom: src.h - b } : { left: l, top: t, cx: Math.max(0, r - l), cy: Math.max(0, b - t) };
}

/** The keys in `next` whose value differs from `prev`. */
export function changedKeys(prev: CropParams, next: Partial<CropParams>): Partial<CropParams> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(next)) if ((prev as unknown as Record<string, unknown>)[k] !== v) out[k] = v;
  return out as Partial<CropParams>;
}

export const sameEdges = (a: Edges, b: Edges) => a.l === b.l && a.t === b.t && a.r === b.r && a.b === b.b;

const minOf = (src: Size): Size => ({ w: Math.min(MIN_SIDE, src.w), h: Math.min(MIN_SIDE, src.h) });

/**
 * The window as it is drawn: every edge inside the picture and the window at least the minimum size. An edge that was clamped is
 * in padding (`padded`); a window with no area (left past the right edge, say) is opened to the minimum next to what is left of it.
 */
export function shown(e: Edges, src: Size): { rect: Edges; padded: { l: boolean; t: boolean; r: boolean; b: boolean } } {
  const min = minOf(src);
  const fit = (lo: number, hi: number, size: number, min: number) => {
    let a = clamp(lo, 0, size);
    let c = clamp(hi, 0, size);
    if (c - a < min) {
      c = Math.min(size, a + min);
      a = c - min;
    }
    return [a, c] as const;
  };
  const [l, r] = fit(e.l, e.r, src.w, min.w);
  const [t, b] = fit(e.t, e.b, src.h, min.h);
  return { rect: { l, t, r, b }, padded: { l: e.l < 0, t: e.t < 0, r: e.r > src.w, b: e.b > src.h } };
}

/** Takes the edges of `next` that moved from `from` (both shown windows) and keeps the true value of the rest, padding included. */
function merge(truth: Edges, from: Edges, next: Edges): Edges {
  return {
    l: next.l !== from.l ? next.l : truth.l,
    t: next.t !== from.t ? next.t : truth.t,
    r: next.r !== from.r ? next.r : truth.r,
    b: next.b !== from.b ? next.b : truth.b,
  };
}

const moves = (h: Handle) => ({ w: h.includes("w"), e: h.includes("e"), n: h.includes("n"), s: h.includes("s") });

/**
 * A handle dragged by (`dx`, `dy`) source pixels from where the gesture began (`truth` are the edges then): the dragged edges follow
 * the pointer, stopped by the picture's edge and by the minimum size, and cannot cross the opposite edge. With `lock` the window keeps
 * its aspect ratio: a corner follows whichever of the two moves is larger and the opposite corner stays; an edge handle changes the
 * other side about the window's middle.
 */
export function resize(truth: Edges, handle: Handle, dx: number, dy: number, src: Size, lock = false): Edges {
  const { rect: d } = shown(truth, src);
  const min = minOf(src);
  const m = moves(handle);
  const w0 = d.r - d.l;
  const h0 = d.b - d.t;
  const next = { ...d };
  if (!lock) {
    if (m.w) next.l = Math.round(clamp(d.l + dx, 0, d.r - min.w));
    if (m.e) next.r = Math.round(clamp(d.r + dx, d.l + min.w, src.w));
    if (m.n) next.t = Math.round(clamp(d.t + dy, 0, d.b - min.h));
    if (m.s) next.b = Math.round(clamp(d.b + dy, d.t + min.h, src.h));
    return merge(truth, d, next);
  }
  const a = w0 / h0;
  // The size the window may have: inside the picture, not under the minimum on either side.
  const sizeFor = (availW: number, availH: number, w: number, h: number) => {
    const s = Math.min(availW / w0, availH / h0);
    const lo = Math.max(min.w / w0, min.h / h0);
    const k = clamp(Math.max(w / w0, h / h0), lo, s);
    return { w: Math.round(k * w0), h: Math.round(k * h0) };
  };
  if ((m.w || m.e) && (m.n || m.s)) {
    // corner: the opposite corner stays
    const ax = m.e ? d.l : d.r;
    const ay = m.s ? d.t : d.b;
    const tx = (m.e ? d.r : d.l) + dx;
    const ty = (m.s ? d.b : d.t) + dy;
    const size = sizeFor(m.e ? src.w - ax : ax, m.s ? src.h - ay : ay, m.e ? tx - ax : ax - tx, m.s ? ty - ay : ay - ty);
    next.l = m.e ? ax : ax - size.w;
    next.r = m.e ? ax + size.w : ax;
    next.t = m.s ? ay : ay - size.h;
    next.b = m.s ? ay + size.h : ay;
  } else if (m.w || m.e) {
    const ax = m.e ? d.l : d.r;
    const tx = (m.e ? d.r : d.l) + dx;
    // the height follows from the width and has to fit the picture
    const size = sizeFor(Math.min(m.e ? src.w - ax : ax, a * src.h), src.h, m.e ? tx - ax : ax - tx, 0);
    const top = Math.round(clamp((d.t + d.b) / 2 - size.h / 2, 0, src.h - size.h));
    next.l = m.e ? ax : ax - size.w;
    next.r = m.e ? ax + size.w : ax;
    next.t = top;
    next.b = top + size.h;
  } else {
    const ay = m.s ? d.t : d.b;
    const ty = (m.s ? d.b : d.t) + dy;
    const size = sizeFor(src.w, Math.min(m.s ? src.h - ay : ay, src.w / a), 0, m.s ? ty - ay : ay - ty);
    const left = Math.round(clamp((d.l + d.r) / 2 - size.w / 2, 0, src.w - size.w));
    next.t = m.s ? ay : ay - size.h;
    next.b = m.s ? ay + size.h : ay;
    next.l = left;
    next.r = left + size.w;
  }
  return merge(truth, d, next);
}

/** The whole window moved by (`dx`, `dy`), kept inside the picture. Every edge becomes the one that shows; nothing moves when there is no room. */
export function pan(truth: Edges, dx: number, dy: number, src: Size): Edges {
  const { rect: d } = shown(truth, src);
  const w = d.r - d.l;
  const h = d.b - d.t;
  const l = Math.round(clamp(d.l + dx, 0, src.w - w));
  const t = Math.round(clamp(d.t + dy, 0, src.h - h));
  if (l === d.l && t === d.t) return truth;
  return { l, t, r: l + w, b: t + h };
}

/**
 * The window scaled by `factor` (below 1 zooms in, above 1 out) about the picture point (`px`, `py`), which stays where it is in the
 * window. The aspect ratio is kept; the result stays inside the picture and over the minimum size. `truth` comes back when nothing changes.
 */
export function zoom(truth: Edges, factor: number, px: number, py: number, src: Size): Edges {
  const { rect: d } = shown(truth, src);
  const min = minOf(src);
  const w0 = d.r - d.l;
  const h0 = d.b - d.t;
  const a = w0 / h0;
  const wMax = Math.min(src.w, a * src.h);
  const wMin = Math.min(wMax, Math.max(min.w, a * min.h));
  let w = Math.round(clamp(w0 * factor, wMin, wMax));
  if (w === w0 && factor !== 1) w = Math.round(clamp(w0 + Math.sign(w0 * factor - w0), wMin, wMax));
  let h = Math.min(src.h, Math.round(w / a));
  // Rounding makes the aspect drift by a pixel: a window within a pixel of the whole picture is the whole picture.
  if (Math.abs(w - src.w) <= 1 && Math.abs(h - src.h) <= 1) {
    w = src.w;
    h = src.h;
  }
  if (w === w0 && h === h0) return truth;
  const k = w / w0;
  const l = Math.round(clamp(px - (px - d.l) * k, 0, src.w - w));
  const t = Math.round(clamp(py - (py - d.t) * (h / h0), 0, src.h - h));
  return { l, t, r: l + w, b: t + h };
}

interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Half the side of a handle's hit area, in screen pixels: shrunk on a small window so the handles do not swallow it. */
export function handleHalf(rect: Box, hit = 12): number {
  return Math.max(3, Math.min(hit / 2, Math.min(rect.w, rect.h) / 4));
}

/**
 * The 8 handles' centres on a window drawn at `rect` (screen pixels). With `bounds` (the picture's rect) a handle on the picture's edge
 * is moved in by its half size, so its whole hit area is on the picture: the preview clips what lies outside it.
 */
export function handlePoints(rect: Box, bounds?: Box, half = 0): Record<Handle, { x: number; y: number }> {
  const { x, y, w, h } = rect;
  const at = (px: number, py: number) =>
    bounds
      ? {
          x: Math.min(Math.max(px, bounds.x + half), Math.max(bounds.x + half, bounds.x + bounds.w - half)),
          y: Math.min(Math.max(py, bounds.y + half), Math.max(bounds.y + half, bounds.y + bounds.h - half)),
        }
      : { x: px, y: py };
  return {
    nw: at(x, y),
    n: at(x + w / 2, y),
    ne: at(x + w, y),
    e: at(x + w, y + h / 2),
    se: at(x + w, y + h),
    s: at(x + w / 2, y + h),
    sw: at(x, y + h),
    w: at(x, y + h / 2),
  };
}

const ORDER: Handle[] = ["nw", "ne", "se", "sw", "n", "e", "s", "w"];

/**
 * What a pointer at (`x`, `y`) grabs on a window drawn at `rect` (screen pixels): a handle (hit area `hit` px square; corners win over
 * edge handles; `bounds` as in `handlePoints`), the inside, or nothing.
 */
export function hitTest(x: number, y: number, rect: Box, hit = 12, bounds?: Box): Handle | "inside" | null {
  const half = handleHalf(rect, hit);
  const points = handlePoints(rect, bounds, half);
  for (const name of ORDER) {
    const p = points[name];
    if (Math.abs(x - p.x) <= half && Math.abs(y - p.y) <= half) return name;
  }
  return x >= rect.x && x <= rect.x + rect.w && y >= rect.y && y <= rect.y + rect.h ? "inside" : null;
}

/** The CSS cursor for what `hitTest` found. */
export function cursorOf(hit: Handle | "inside" | null): string {
  switch (hit) {
    case "nw":
    case "se":
      return "nwse-resize";
    case "ne":
    case "sw":
      return "nesw-resize";
    case "n":
    case "s":
      return "ns-resize";
    case "e":
    case "w":
      return "ew-resize";
    case "inside":
      return "move";
    default:
      return "default";
  }
}

/** The wheel's `deltaY` as a zoom factor: 100 (one notch) is about 16 %; a wheel that counts in lines or pages is scaled to pixels first. */
export function wheelFactor(deltaY: number, deltaMode = 0): number {
  const px = deltaY * (deltaMode === 1 ? 16 : deltaMode === 2 ? 400 : 1);
  return Math.exp(clamp(px, -300, 300) * 0.0015);
}

/** The arrow key's step in source pixels, or `null` for another key. */
export function nudgeOf(key: string, shift: boolean): { dx: number; dy: number } | null {
  const n = shift ? NUDGE_FAST : NUDGE;
  switch (key) {
    case "ArrowLeft":
      return { dx: -n, dy: 0 };
    case "ArrowRight":
      return { dx: n, dy: 0 };
    case "ArrowUp":
      return { dx: 0, dy: -n };
    case "ArrowDown":
      return { dx: 0, dy: n };
    default:
      return null;
  }
}
