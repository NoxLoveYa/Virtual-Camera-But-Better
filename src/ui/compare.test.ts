/**
 * Checks of the split-wipe Compare logic: the bar's position from a pointer and from keys, the clamps, the orientation switch, the
 * picture rect and the saved state. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { BIG_STEP, DEFAULT_SPLIT, LABEL_MAX_PX, LABEL_OFFSET_PX, LABEL_PX, STEP, clampPos, compareLabels, comparePoint, isOrientKey, labelPx, labelShown, pictureRect, posFromKey, posFromPointer, readSaved, retargetCompare, swap } from "./compare";
import { samePoint, type PreviewPoint } from "./previewFrame";

function eq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

// ---- clamps
eq(clampPos(0.5), 0.5, "inside stays");
eq(clampPos(-0.3), 0, "below the picture");
eq(clampPos(1.7), 1, "above the picture");
eq(clampPos(Number.NaN), 0.5, "not a number falls back to the middle");
eq(clampPos(Number.POSITIVE_INFINITY), 0.5, "infinity falls back to the middle");
eq(clampPos(0.12345), 0.123, "rounded to a tenth of a percent");

// ---- orientation
eq(swap("vertical"), "horizontal", "vertical to horizontal");
eq(swap("horizontal"), "vertical", "horizontal to vertical");
eq(DEFAULT_SPLIT, { pos: 0.5, orientation: "vertical" }, "the default is a centred vertical bar");

// ---- the picture rect: the box letterboxes the canvas
eq(pictureRect({ width: 640, height: 360 }, { width: 640, height: 360 }), { left: 0, top: 0, width: 640, height: 360 }, "16:9 fills the box");
eq(pictureRect({ width: 640, height: 360 }, { width: 320, height: 240 }), { left: 80, top: 0, width: 480, height: 360 }, "4:3 is pillarboxed");
eq(pictureRect({ width: 640, height: 360 }, { width: 100, height: 400 }), { left: 275, top: 0, width: 90, height: 360 }, "a tall picture is pillarboxed");
eq(pictureRect({ width: 640, height: 360 }, { width: 1280, height: 360 }), { left: 0, top: 90, width: 640, height: 180 }, "a wide picture is letterboxed");
eq(pictureRect({ width: 640, height: 360 }, { width: 0, height: 0 }), { left: 0, top: 0, width: 640, height: 360 }, "no picture yet: the box");

// ---- the bar from a pointer: a picture at (100, 50), 400 x 200 on the screen
const rect = { left: 100, top: 50, width: 400, height: 200 };
eq(posFromPointer(300, 999, rect, "vertical"), 0.5, "the middle, vertical (y ignored)");
eq(posFromPointer(200, 0, rect, "vertical"), 0.25, "a quarter, vertical");
eq(posFromPointer(0, 0, rect, "vertical"), 0, "left of the picture clamps to 0");
eq(posFromPointer(900, 0, rect, "vertical"), 1, "right of the picture clamps to 1");
eq(posFromPointer(0, 100, rect, "horizontal"), 0.25, "a quarter, horizontal (x ignored)");
eq(posFromPointer(0, 250, rect, "horizontal"), 1, "the bottom edge");
eq(posFromPointer(0, 0, rect, "horizontal"), 0, "above the picture clamps to 0");
eq(posFromPointer(5, 5, { left: 0, top: 0, width: 0, height: 0 }, "vertical"), 0.5, "an empty rect gives the middle");

// ---- keys
eq(posFromKey(0.5, "ArrowRight", false), 0.52, "right: +2 %");
eq(posFromKey(0.5, "ArrowLeft", false), 0.48, "left: -2 %");
eq(posFromKey(0.5, "ArrowDown", false), 0.52, "down: +2 %");
eq(posFromKey(0.5, "ArrowUp", false), 0.48, "up: -2 %");
eq(posFromKey(0.5, "ArrowRight", true), 0.6, "Shift: +10 %");
eq(posFromKey(0.5, "ArrowLeft", true), 0.4, "Shift: -10 %");
eq(posFromKey(0.99, "ArrowRight", false), 1, "no step past the edge");
eq(posFromKey(0.01, "ArrowLeft", true), 0, "no step below the edge");
eq(posFromKey(0.3, "Home", false), 0, "Home goes to the start");
eq(posFromKey(0.3, "End", false), 1, "End goes to the end");
eq(posFromKey(0.3, "a", false), null, "another key is not the bar's");
eq(posFromKey(0.3, "Enter", false), null, "Enter is not the bar's");
let p = 0.5;
for (let i = 0; i < 7; i++) p = posFromKey(p, "ArrowRight", false)!;
eq(p, 0.64, "seven steps do not drift");
eq([STEP, BIG_STEP], [0.02, 0.1], "the steps");

// ---- the labels
eq(labelShown("before", 0.5, 640), true, "both labels with the bar in the middle");
eq(labelShown("after", 0.5, 640), true, "both labels with the bar in the middle");
eq(labelShown("before", 0.1, 640), false, "Before hides on a sliver (64 px)");
eq(labelShown("before", 0.12, 640), true, "Before shows from 72 px");
eq(labelShown("after", 0.9, 640), false, "After hides on a sliver");
eq(labelShown("after", 0.5, 100), false, "a tiny picture has no room for a label");
eq(labelShown("before", 1, 640), true, "Before fills the picture");
eq(labelShown("after", 1, 640), false, "After is gone at 1");
eq(labelShown("before", 0, 640), false, "Before is gone at 0");
eq(LABEL_PX, 72, "the room a label needs");

// ---- the point Compare takes from the selection
const chain = [{ id: "a" }, { id: "b" }];
eq(comparePoint("filter:a", chain), { compare: "a" }, "a filter selected: its own compare");
eq(comparePoint("camera", chain), "compare", "camera selected: the camera compare");
eq(comparePoint("controls", chain), "compare", "capture controls selected: the camera compare");
eq(comparePoint("output", chain), "compare", "output selected: the camera compare");
eq(comparePoint("filter:gone", chain), "compare", "a filter that is not in the chain: the camera compare");
eq(comparePoint("filter:a", []), "compare", "an empty chain: the camera compare");

// ---- Compare follows the selection while it is on, and only while it is on
eq(retargetCompare(null, "filter:a", chain), undefined, "Compare off: nothing follows");
eq(retargetCompare({ filter: "a" }, "filter:b", chain), undefined, "the Before hold is no Compare: nothing follows");
eq(retargetCompare("compare", "camera", chain), undefined, "on, camera selected: already right");
eq(retargetCompare({ compare: "a" }, "filter:a", chain), undefined, "on, the same filter: already right");
eq(retargetCompare("compare", "filter:a", chain), { compare: "a" }, "on with the camera, a filter gets selected");
eq(retargetCompare({ compare: "a" }, "filter:b", chain), { compare: "b" }, "on, another filter gets selected");
eq(retargetCompare({ compare: "a" }, "output", chain), "compare", "on, a section gets selected: back to the camera compare");
eq(retargetCompare({ compare: "a" }, "filter:b", [{ id: "b" }]), { compare: "b" }, "the compared filter was removed and the next one is selected");
eq(retargetCompare({ compare: "a" }, "output", []), "compare", "the compared filter was removed and nothing is left");
eq(retargetCompare({ compare: "a" }, "filter:a", [{ id: "b" }]), "compare", "a selection that names a removed filter does not compare it");
// a run of selections with Compare on, as the store applies them
let point: PreviewPoint = "compare";
const seen: PreviewPoint[] = [];
for (const selected of ["filter:a", "filter:a", "filter:b", "controls", "filter:b"]) {
  point = retargetCompare(point, selected, chain) ?? point;
  seen.push(point);
}
eq(seen, [{ compare: "a" }, { compare: "a" }, { compare: "b" }, "compare", { compare: "b" }], "the point tracks the selection");
eq(samePoint(seen[1], seen[0]), true, "no change of selection, no new point");

// ---- the labels of the two sides
const steps = [{ id: "a", label: "Color" }, { id: "b", label: "Crop/Pad" }];
eq(compareLabels("compare", steps), { before: "Before", after: "After" }, "the camera compare keeps its labels");
eq(compareLabels(null, steps), { before: "Before", after: "After" }, "no compare, the plain labels");
eq(compareLabels({ compare: "b" }, steps), { before: "Before Crop/Pad", after: "After" }, "a filter compare names the filter");
eq(compareLabels({ compare: "gone" }, steps), { before: "Before", after: "After" }, "a filter that left: the plain labels");
eq([labelPx("Before"), labelPx("After")], [LABEL_PX, LABEL_PX], "the short labels need the usual room");
eq(labelPx("Before Crop/Pad") > LABEL_PX, true, "a longer label needs more room");
eq(labelPx("Before " + "x".repeat(200)), LABEL_MAX_PX + LABEL_OFFSET_PX, "a very long label stops at the maximum and its offset");
eq(labelShown("before", 0.2, 640, labelPx("Before Crop/Pad")), true, "128 px is room for a filter label");
eq(labelShown("before", 0.12, 640, labelPx("Before Crop/Pad")), false, "77 px is not");
eq(labelShown("before", 0.12, 640), true, "the plain label still shows from 72 px");

// ---- the saved state
eq(readSaved(null), DEFAULT_SPLIT, "nothing saved");
eq(readSaved("{"), DEFAULT_SPLIT, "broken JSON");
eq(readSaved("null"), DEFAULT_SPLIT, "null");
eq(readSaved('{"pos":0.25,"orientation":"horizontal"}'), { pos: 0.25, orientation: "horizontal" }, "a saved bar");
eq(readSaved('{"pos":4,"orientation":"sideways"}'), { pos: 1, orientation: "vertical" }, "a position clamps, an unknown orientation is vertical");
eq(readSaved('{"pos":"x"}'), DEFAULT_SPLIT, "a position that is not a number");

// ---- the orientation key
const key = (k: string, mods: { ctrlKey?: boolean; metaKey?: boolean; altKey?: boolean } = {}) => ({ key: k, ctrlKey: false, metaKey: false, altKey: false, ...mods });
eq(isOrientKey(key("o"), false), true, "o");
eq(isOrientKey(key("O"), false), true, "O (Shift or caps lock)");
eq(isOrientKey(key("o"), true), false, "typing in a field");
eq(isOrientKey(key("o", { ctrlKey: true }), false), false, "Ctrl+O is the browser's");
eq(isOrientKey(key("o", { altKey: true }), false), false, "Alt+O");
eq(isOrientKey(key("p"), false), false, "another key");
console.log("compare: ok");
