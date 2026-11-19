/**
 * Checks of the pure layout logic: the saved `ui`, the widths of the three regions and the splitter keys, and the
 * selection rule. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import {
  CENTER_MIN,
  DEFAULT_UI,
  INSPECTOR_MAX,
  INSPECTOR_MIN,
  NAV_MAX,
  NAV_MIN,
  RAIL_WIDTH,
  effectiveWidths,
  filterSelection,
  isRail,
  normalizeUi,
  selectedFilterId,
  selectionAfter,
  widthAfterKey,
  widthFromDrag,
  widthRange,
} from "./layout";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

// ---- the saved state ----
eq(JSON.stringify(normalizeUi(undefined)), JSON.stringify(DEFAULT_UI), "no saved ui is the default");
eq(JSON.stringify(normalizeUi(null)), JSON.stringify(DEFAULT_UI), "null is the default");
eq(JSON.stringify(normalizeUi("x")), JSON.stringify(DEFAULT_UI), "a string is the default");
// The state an older build saved: the keys are dropped, nothing throws, everything is auto.
const old = normalizeUi({ split: 0.6, camera: false, controls: false, output: false, advanced: true });
eq(JSON.stringify(old), JSON.stringify({ ...DEFAULT_UI, advanced: true }), "an old ui keeps only Advanced");
eq(Object.keys(old).join(), Object.keys(DEFAULT_UI).join(), "no old key survives");
eq(normalizeUi({ navWidth: 240 }).navWidth, 240, "a width in range is kept");
eq(normalizeUi({ navWidth: 183 }).navWidth, null, "a navigator width below its range is auto");
eq(normalizeUi({ navWidth: 321 }).navWidth, null, "a navigator width above its range is auto");
eq(normalizeUi({ navWidth: NAV_MIN }).navWidth, NAV_MIN, "the lower limit is in range");
eq(normalizeUi({ navWidth: NAV_MAX }).navWidth, NAV_MAX, "the upper limit is in range");
eq(normalizeUi({ inspectorWidth: 420 }).inspectorWidth, 420, "an inspector width in range is kept");
eq(normalizeUi({ inspectorWidth: 299 }).inspectorWidth, null, "an inspector width below its range is auto");
eq(normalizeUi({ inspectorWidth: 561 }).inspectorWidth, null, "an inspector width above its range is auto");
eq(normalizeUi({ inspectorWidth: 420.4 }).inspectorWidth, 420, "a width is whole pixels");
eq(normalizeUi({ navWidth: "240", inspectorWidth: NaN }).navWidth, null, "a string width is auto");
eq(normalizeUi({ navWidth: "240", inspectorWidth: NaN }).inspectorWidth, null, "NaN is auto");
eq(normalizeUi({ navWidth: Infinity }).navWidth, null, "Infinity is auto");
eq(normalizeUi({ navRail: true }).navRail, true, "the rail flag is kept");
eq(normalizeUi({ navRail: "yes" }).navRail, false, "a non-boolean rail flag is off");
eq(normalizeUi({ advanced: "yes" }).advanced, false, "a non-boolean Advanced is closed");
for (const s of ["camera", "controls", "output", "filter:abc"]) eq(normalizeUi({ selected: s }).selected, s, `selection ${s} is kept`);
for (const s of ["", "filter:", "filters", "Camera", 3, null, {}]) eq(normalizeUi({ selected: s }).selected, "camera", `junk selection ${JSON.stringify(s)} is camera`);

eq(selectedFilterId("filter:x1"), "x1", "the id of a filter selection");
eq(selectedFilterId("camera"), null, "a section has no filter id");
eq(filterSelection("x1"), "filter:x1", "the selection of a filter");

// ---- widths ----
const at = (w: number, nav: number | null = null, insp: number | null = null) => effectiveWidths(w, nav, insp);
// Auto widths: nav clamp(200, 17vw, 260), inspector clamp(340, 25vw, 460); the centre gets the rest minus two hairlines.
// Below 1100 px the navigator is the 52 px icon rail, so the preview keeps the room.
const w900 = at(900);
eq(w900.nav, RAIL_WIDTH, "900: the navigator is the rail");
eq(w900.rail, true, "900: rail flag");
eq(w900.inspector, 340, "900: inspector 340");
eq(w900.center, 506, "900: centre 506");
eq(at(1099).nav, RAIL_WIDTH, "1099: rail");
eq(at(1099).rail, true, "1099: rail flag");
eq(at(1099).center, 1097 - 52 - 340, "1099: centre is the rest");
eq(at(1100).rail, false, "1100: the full navigator");
eq(at(1100).nav, 200, "1100: navigator 200");
eq(isRail(1099, false), true, "isRail: 1099");
eq(isRail(1100, false), false, "isRail: 1100");
eq(isRail(1920, true), true, "isRail: collapsed at any width");
// A collapsed navigator at a wide window: the rail, and the inspector keeps its width.
eq(effectiveWidths(1400, null, null, true).nav, RAIL_WIDTH, "1400 collapsed: rail");
eq(effectiveWidths(1400, null, null, true).inspector, 350, "1400 collapsed: inspector auto");
eq(effectiveWidths(1400, 260, null, true).nav, RAIL_WIDTH, "a saved navigator width does not change the rail");
eq(effectiveWidths(1400, 260, null, false).nav, 260, "... and is back when the navigator is full");
eq(widthRange("inspector", 900, w900.nav).max, 486, "900: the inspector may take the room the rail leaves");
const w1100 = at(1100);
eq(w1100.nav, 200, "1100: navigator 200");
eq(w1100.inspector, 340, "1100: inspector 340");
eq(w1100.center, 1100 - 2 - 200 - 340, "1100: centre is the rest");
const w1200 = at(1200);
eq(w1200.nav, 204, "1200: navigator 204");
eq(w1200.inspector, 340, "1200: inspector 340");
eq(w1200.center, 654, "1200: centre 654");
const w1920 = at(1920);
eq(w1920.nav, 260, "1920: navigator 260");
eq(w1920.inspector, 460, "1920: inspector 460");
eq(w1920.center, 1198, "1920: centre 1198");
// Saved widths are used and clamped to the room; the saved value itself is not changed (the function is pure).
eq(at(1200, 240, 420).nav, 240, "a saved navigator width is used");
eq(at(1200, 240, 420).inspector, 420, "a saved inspector width is used");
eq(at(1100, 320, 560).nav, 320, "1100: a wide saved navigator fits");
eq(at(1100, 320, 560).inspector, 1098 - 320 - CENTER_MIN, "1100: ... and the saved inspector gives way to keep the centre");
eq(at(1100, 320, 560).center, CENTER_MIN, "1100: ... at its minimum");
eq(at(1100, 400, null).nav, 320, "1100: a navigator above its range is held to the range");
eq(at(900, 320, 560).inspector, 486, "900: a wide saved inspector is held to the room the rail leaves");
eq(at(1000, null, 560).inspector, 560, "1000: the rail leaves room for the widest inspector");
eq(at(800, null, 560).inspector, 800 - 2 - RAIL_WIDTH - CENTER_MIN, "800: a wide saved inspector is held to the room");
eq(at(1400, null, 560).inspector, 560, "1400: a wide saved inspector fits");
for (const w of [900, 1000, 1099, 1100, 1200, 1400, 1920, 2560]) {
  for (const [n, i] of [[null, null], [NAV_MAX, INSPECTOR_MAX], [NAV_MIN, INSPECTOR_MIN]] as const) {
    const r = at(w, n, i);
    if (r.rail && r.nav !== RAIL_WIDTH) throw new Error(`${w}: the rail is ${r.nav}`);
    if (!r.rail && (r.nav < NAV_MIN || r.nav > NAV_MAX)) throw new Error(`${w}: navigator ${r.nav} outside its range`);
    if (r.inspector < INSPECTOR_MIN || r.inspector > INSPECTOR_MAX) throw new Error(`${w}: inspector ${r.inspector} outside its range`);
    if (r.center < CENTER_MIN) throw new Error(`${w}: centre ${r.center} below its minimum`);
    eq(r.nav + r.inspector + r.center + 2, w, `${w}: the three regions and two hairlines fill the window`);
  }
}

// ---- ranges, drags and keys ----
const navRange = widthRange("nav", 1200, 340);
eq(navRange.min, NAV_MIN, "navigator range: min");
eq(navRange.max, NAV_MAX, "navigator range: max at 1200");
eq(widthRange("nav", 1100, 538).max, 200, "navigator range at 1100: the room left");
const inspRange = widthRange("inspector", 1200, 204);
eq(inspRange.min, INSPECTOR_MIN, "inspector range: min");
eq(inspRange.max, INSPECTOR_MAX, "inspector range: max at 1200");
eq(widthRange("inspector", 1100, 200).max, 538, "inspector range at 1100: the room left");

eq(widthFromDrag("inspector", 340, -80, inspRange), 420, "dragging the inspector line 80 px left widens it by 80");
eq(widthFromDrag("inspector", 340, 80, inspRange), 300, "dragging it right narrows it, to the minimum");
eq(widthFromDrag("inspector", 340, -1000, inspRange), INSPECTOR_MAX, "a drag far left stops at the maximum");
eq(widthFromDrag("nav", 204, 30, navRange), 234, "dragging the navigator line right widens it");
eq(widthFromDrag("nav", 204, -100, navRange), NAV_MIN, "a drag far left stops at the minimum");

eq(widthAfterKey("nav", "ArrowRight", false, 204, navRange), 220, "nav ArrowRight +16");
eq(widthAfterKey("nav", "ArrowLeft", false, 204, navRange), 188, "nav ArrowLeft -16");
eq(widthAfterKey("nav", "ArrowRight", true, 204, navRange), 268, "nav Shift+ArrowRight +64");
eq(widthAfterKey("nav", "ArrowLeft", true, 204, navRange), NAV_MIN, "nav keys stop at the minimum");
eq(widthAfterKey("nav", "Home", false, 204, navRange), NAV_MIN, "nav Home");
eq(widthAfterKey("nav", "End", false, 204, navRange), NAV_MAX, "nav End");
eq(widthAfterKey("inspector", "ArrowLeft", false, 340, inspRange), 356, "inspector ArrowLeft widens it (mirrored)");
eq(widthAfterKey("inspector", "ArrowRight", false, 340, inspRange), 324, "inspector ArrowRight narrows it");
eq(widthAfterKey("inspector", "ArrowLeft", true, 340, inspRange), 404, "inspector Shift+ArrowLeft +64");
eq(widthAfterKey("inspector", "Home", false, 340, inspRange), INSPECTOR_MAX, "inspector Home is the left-most line");
eq(widthAfterKey("inspector", "End", false, 340, inspRange), INSPECTOR_MIN, "inspector End is the right-most line");
eq(widthAfterKey("nav", "Enter", false, 204, navRange), null, "Enter is back to auto");
eq(widthAfterKey("inspector", "Enter", false, 340, inspRange), null, "Enter is back to auto (inspector)");
eq(widthAfterKey("nav", "a", false, 204, navRange), undefined, "other keys are not handled");

// ---- the selection rule ----
const chain = (...ids: string[]) => ids.map((id) => ({ id }));
const abc = chain("a", "b", "c");
eq(selectionAfter(abc, abc, "filter:b"), "filter:b", "an untouched selection stays");
eq(selectionAfter(abc, chain("b", "c"), "filter:a"), "filter:b", "removing the first selects the next");
eq(selectionAfter(abc, chain("a", "c"), "filter:b"), "filter:c", "removing a middle one selects the next");
eq(selectionAfter(abc, chain("a", "b"), "filter:c"), "filter:b", "removing the last selects the previous");
eq(selectionAfter(chain("a"), [], "filter:a"), "output", "removing the only one selects Output");
eq(selectionAfter(abc, chain("a", "c"), "filter:c"), "filter:c", "removing another filter leaves the selection");
eq(selectionAfter(abc, chain("c", "a", "b"), "filter:b"), "filter:b", "a reorder leaves the selection (it follows the id)");
eq(selectionAfter(abc, chain("x", "y"), "filter:b"), "filter:y", "a replaced chain: the filter at the old index");
eq(selectionAfter(abc, chain("x"), "filter:c"), "filter:x", "a shorter replaced chain: the last one");
eq(selectionAfter(abc, [], "filter:b"), "output", "an emptied chain selects Output");
eq(selectionAfter(abc, chain("x"), "controls"), "controls", "a section is never changed");
eq(selectionAfter(abc, [], "camera"), "camera", "a section stays when the chain empties");
eq(selectionAfter(abc, chain("x"), "filter:zzz"), "camera", "a selection that named no filter is camera");

console.log("ok");
