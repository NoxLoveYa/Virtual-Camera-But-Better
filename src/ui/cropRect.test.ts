/**
 * Checks of the crop frame's maths: params to window and back, handles, pan, zoom, the aspect lock, padding, hit-testing, the
 * wheel and arrow keys. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import {
  MIN_SIDE,
  changedKeys,
  cursorOf,
  edgesOf,
  handlePoints,
  hitTest,
  nudgeOf,
  pan,
  paramsOf,
  readCrop,
  resize,
  shown,
  wheelFactor,
  zoom,
  type CropParams,
  type Edges,
} from "./cropRect";

function eq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}
const E = (l: number, t: number, r: number, b: number): Edges => ({ l, t, r, b });
const src = { w: 1000, h: 500 };
eq(MIN_SIDE, 16, "minimum side");

// ---- params and window
const rel = (left: number, top: number, right: number, bottom: number): CropParams => ({ relative: true, left, top, right, bottom, cx: 0, cy: 0 });
const abs = (left: number, top: number, cx: number, cy: number): CropParams => ({ relative: false, left, top, right: 0, bottom: 0, cx, cy });
eq(readCrop({}), rel(0, 0, 0, 0), "defaults");
eq(readCrop({ relative: false, left: 3.6, cx: -5, top: "x", right: NaN }), { relative: false, left: 4, top: 0, right: 0, bottom: 0, cx: 0, cy: 0 }, "odd values are the defaults");
eq(edgesOf(rel(100, 50, 200, 25), src), E(100, 50, 800, 475), "relative window");
eq(edgesOf(rel(-20, 0, -30, 0), src), E(-20, 0, 1030, 500), "padding is outside the picture");
eq(edgesOf(abs(10, 20, 300, 100), src), E(10, 20, 310, 120), "absolute window");
eq(edgesOf(abs(10, 20, 0, 0), src), E(10, 20, 1000, 500), "an absolute size of 0 runs to the edge");
eq(edgesOf(abs(0, 0, 0, 0), src), E(0, 0, 1000, 500), "the default shows the whole picture in both modes");
eq(paramsOf(E(100, 50, 800, 475), true, src), { left: 100, top: 50, right: 200, bottom: 25 }, "relative params");
eq(paramsOf(E(100, 50, 800, 475), false, src), { left: 100, top: 50, cx: 700, cy: 425 }, "absolute params");
eq(paramsOf(E(10.4, 20.6, 300.2, 100.5), true, src), { left: 10, top: 21, right: 700, bottom: 399 }, "whole pixels");
for (const e of [E(100, 50, 800, 475), E(0, 0, 1000, 500), E(5, 5, 21, 21)]) {
  eq(edgesOf(readCrop(paramsOf(e, true, src)), src), e, "relative round trip");
  eq(edgesOf(readCrop({ ...paramsOf(e, false, src), relative: false }), src), e, "absolute round trip");
}
eq(changedKeys(rel(1, 2, 3, 4), { left: 1, top: 9, right: 3, bottom: 8 }), { top: 9, bottom: 8 }, "only the keys that differ");

// ---- the window as drawn
eq(shown(E(100, 50, 800, 475), src), { rect: E(100, 50, 800, 475), padded: { l: false, t: false, r: false, b: false } }, "a window inside is as it is");
eq(shown(E(-20, 0, 1050, 500), src), { rect: E(0, 0, 1000, 500), padded: { l: true, t: false, r: true, b: false } }, "padding is drawn at the picture edge");
eq(shown(E(990, 0, 1000, 500), src).rect, E(984, 0, 1000, 500), "a window under the minimum is opened");
eq(shown(E(1500, 0, 1600, 500), src).rect, E(984, 0, 1000, 500), "a window past the edge is brought back");
eq(shown(E(0, 0, 4, 3), { w: 8, h: 6 }).rect, E(0, 0, 8, 6), "a picture smaller than the minimum is the minimum");

// ---- handles, no lock
const w = E(100, 50, 800, 475);
eq(resize(w, "e", 50, 99, src), E(100, 50, 850, 475), "east moves the right edge only");
eq(resize(w, "e", 500, 0, src), E(100, 50, 1000, 475), "east stops at the picture edge");
eq(resize(w, "e", -1000, 0, src), E(100, 50, 116, 475), "east stops at the minimum width");
eq(resize(w, "w", -500, 0, src), E(0, 50, 800, 475), "west stops at the picture edge");
eq(resize(w, "w", 1000, 0, src), E(784, 50, 800, 475), "west stops at the minimum width");
eq(resize(w, "n", 0, -80, src), E(100, 0, 800, 475), "north stops at the picture edge");
eq(resize(w, "s", 0, -1000, src), E(100, 50, 800, 66), "south stops at the minimum height");
eq(resize(w, "nw", 20, 10, src), E(120, 60, 800, 475), "a corner moves two edges");
eq(resize(w, "se", -20.4, 10.6, src), E(100, 50, 780, 486), "whole pixels");
eq(resize(w, "e", 0, 0, src), w, "no move, no change");
// a negative (padded) edge is kept unless it is dragged
const padded = E(-30, 0, 1000, 500);
eq(resize(padded, "e", -100, 0, src), E(-30, 0, 900, 500), "another edge dragged: the padded one keeps its value");
eq(resize(padded, "w", -50, 0, src), padded, "a padded edge dragged outward stays padded");
eq(resize(padded, "w", 40, 0, src), E(40, 0, 1000, 500), "a padded edge dragged in is set inside the picture");
eq(resize(E(0, 0, 1000, 500), "w", -50, 0, src), E(0, 0, 1000, 500), "no padding is ever written by a drag");
eq(resize(E(0, 0, 1100, 500), "s", 0, -100, src), E(0, 0, 1100, 400), "a padded right edge stays when the bottom moves");

// ---- handles, aspect lock (2:1)
const full = E(0, 0, 1000, 500);
eq(resize(full, "se", -200, -100, src, true), E(0, 0, 800, 400), "a corner keeps the aspect");
eq(resize(full, "se", -400, -100, src, true), E(0, 0, 800, 400), "a corner follows the larger move");
eq(resize(E(200, 100, 800, 400), "nw", 100, 50, src, true), E(300, 150, 800, 400), "the opposite corner stays");
eq(resize(full, "se", -10000, -10000, src, true), E(0, 0, 32, 16), "the lock stops at the minimum on both sides");
eq(resize(E(0, 0, 500, 250), "se", 10000, 0, src, true), E(0, 0, 1000, 500), "the lock stops at the picture");
eq(resize(E(0, 0, 500, 250), "e", 100, 0, src, true), E(0, 0, 600, 300), "an edge keeps the aspect about the middle");
{
  const r = resize(E(0, 100, 500, 350), "n", 0, -40, src, true);
  eq([r.b, r.t, r.r - r.l, r.b - r.t], [350, 60, 580, 290], "north with the lock: taller by 40, wider by 80 about the middle, inside the picture");
}

// ---- pan
eq(pan(w, 500, 0, src), E(300, 50, 1000, 475), "pan stops at the right edge");
eq(pan(w, -1000, -1000, src), E(0, 0, 700, 425), "pan stops at the top left");
eq(pan(w, 10, 5, src), E(110, 55, 810, 480), "pan keeps the size");
eq(pan(full, 20, 20, src), full, "no room to pan: nothing changes");
eq(pan(E(-30, 10, 900, 400), 50, 0, src), E(50, 10, 950, 400), "a pan sets a padded edge inside the picture");
eq(pan(E(-30, 0, 1000, 500), 5, 5, src), E(-30, 0, 1000, 500), "a window with no room keeps its padding");

// ---- zoom about the pointer
eq(zoom(full, 0.5, 500, 250, src), E(250, 125, 750, 375), "zoom in about the middle");
eq(zoom(full, 0.5, 0, 0, src), E(0, 0, 500, 250), "zoom in about the corner keeps the corner");
eq(zoom(full, 0.5, 1000, 500, src), E(500, 250, 1000, 500), "zoom in about the far corner");
eq(zoom(E(250, 125, 750, 375), 2, 500, 250, src), full, "zoom back out");
eq(zoom(full, 2, 500, 250, src), full, "zoom out of the whole picture changes nothing");
eq(zoom(full, 0.0001, 500, 250, src), E(484, 242, 516, 258), "zoom in stops at the minimum, aspect kept");
{
  // zooming in and out again returns the whole picture, though rounding drifts the aspect by a pixel on the way
  let e = full;
  for (let i = 0; i < 3; i++) e = zoom(e, 0.8607, 320, 180, src);
  for (let i = 0; i < 4; i++) e = zoom(e, 1.1618, 320, 180, src);
  eq(e, full, "zoom in three notches and out four: the whole picture");
  eq(zoom(E(0, 0, 999, 500), 1.2, 500, 250, src), full, "a window a pixel short of the picture snaps to it");
}
{
  const z = zoom(full, 1 / 3, 300, 100, src);
  const ww = z.r - z.l;
  const hh = z.b - z.t;
  eq(Math.abs(ww / hh - 2) < 0.02, true, `zoom keeps the aspect (${ww}x${hh})`);
  eq(z.l >= 0 && z.t >= 0 && z.r <= 1000 && z.b <= 500, true, "zoom stays inside the picture");
  // the point under the pointer stays about where it was in the window
  eq(Math.abs((300 - z.l) / ww - 0.3) < 0.02, true, "the pointer's point keeps its place in the window");
}
{
  const z = zoom(E(100, 50, 400, 200), 0.999, 250, 125, src);
  eq(z.r - z.l, 299, "a tiny zoom moves by at least a pixel");
}

// ---- hit-testing
const rect = { x: 100, y: 100, w: 200, h: 100 };
eq(Object.keys(handlePoints(rect)).length, 8, "8 handles");
eq(hitTest(100, 100, rect), "nw", "the top left corner");
eq(hitTest(105, 105, rect), "nw", "within 6 px of a corner");
eq(hitTest(107, 107, rect), "inside", "past the hit area it is the inside");
eq(hitTest(200, 100, rect), "n", "the top middle");
eq(hitTest(300, 150, rect), "e", "the right middle");
eq(hitTest(200, 200, rect), "s", "the bottom middle");
eq(hitTest(100, 150, rect), "w", "the left middle");
eq(hitTest(300, 200, rect), "se", "the bottom right corner");
eq(hitTest(100, 200, rect), "sw", "the bottom left corner");
eq(hitTest(300, 100, rect), "ne", "the top right corner");
eq(hitTest(200, 150, rect), "inside", "the middle");
eq(hitTest(50, 50, rect), null, "outside");
eq(hitTest(95, 100, rect), "nw", "the hit area reaches outside the window");
eq(hitTest(4, 4, { x: 0, y: 0, w: 8, h: 8 }), "inside", "a tiny window keeps an inside");
eq(hitTest(1, 1, { x: 0, y: 0, w: 8, h: 8 }), "nw", "a tiny window's corner");
// a handle on the picture's edge is moved in by its half size, so the whole hit area is on the picture (the preview clips the rest)
{
  const picture = { x: 0, y: 0, w: 400, h: 200 };
  const edge = { x: 0, y: 0, w: 400, h: 200 };
  const pts = handlePoints(edge, picture, 6);
  eq(pts.nw, { x: 6, y: 6 }, "nw moved in");
  eq(pts.se, { x: 394, y: 194 }, "se moved in");
  eq(pts.e, { x: 394, y: 100 }, "e moved in on x only");
  eq(pts.n, { x: 200, y: 6 }, "n moved in on y only");
  eq(hitTest(2, 2, edge, 12, picture), "nw", "the corner of the picture grabs the corner handle");
  eq(hitTest(0, 100, edge, 12, picture), "w", "the middle of the picture's left edge grabs the west handle");
  eq(hitTest(398, 100, edge, 12, picture), "e", "and the east one");
  eq(handlePoints({ x: 100, y: 50, w: 200, h: 100 }, picture, 6).nw, { x: 100, y: 50 }, "a window inside the picture is not moved");
}
eq(cursorOf("nw"), "nwse-resize", "cursor nw");
eq(cursorOf("sw"), "nesw-resize", "cursor sw");
eq(cursorOf("n"), "ns-resize", "cursor n");
eq(cursorOf("e"), "ew-resize", "cursor e");
eq(cursorOf("inside"), "move", "cursor inside");
eq(cursorOf(null), "default", "cursor outside");

// ---- wheel and keys
eq(wheelFactor(0), 1, "no wheel");
eq(wheelFactor(-100) < 1 && wheelFactor(100) > 1, true, "wheel up zooms in");
eq(Math.round(wheelFactor(-100) * 1000), 861, "one notch is about 16 %");
eq(wheelFactor(-5000), wheelFactor(-300), "a huge delta is capped");
eq(wheelFactor(-3, 1), wheelFactor(-48), "lines count 16 px");
eq(nudgeOf("ArrowLeft", false), { dx: -1, dy: 0 }, "left");
eq(nudgeOf("ArrowRight", true), { dx: 10, dy: 0 }, "shift right");
eq(nudgeOf("ArrowUp", false), { dx: 0, dy: -1 }, "up");
eq(nudgeOf("ArrowDown", true), { dx: 0, dy: 10 }, "shift down");
eq(nudgeOf("a", false), null, "another key");
console.log("cropRect: ok");
