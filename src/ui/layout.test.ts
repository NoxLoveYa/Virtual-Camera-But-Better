/**
 * Checks of the pure layout logic (W3): split limits and keys, and the normalising of the saved `ui`.
 * `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { DEFAULT_SPLIT, DEFAULT_UI, MIN_LEFT, MIN_RIGHT, clampSplit, normalizeUi, splitAfterKey, splitBounds, splitFromWidth } from "./layout";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}
const near = (actual: number, expected: number, what: string, eps = 1e-9) => {
  if (Math.abs(actual - expected) > eps) throw new Error(`${what}: expected ${expected}, got ${actual}`);
};

// Limits: left >= 440 px, right >= 420 px.
const b = splitBounds(1200);
near(b.min * 1200, MIN_LEFT, "the left limit in px");
near((1 - b.max) * 1200, MIN_RIGHT, "the right limit in px");
near(clampSplit(0.1, 1200), b.min, "below the left limit");
near(clampSplit(0.95, 1200), b.max, "above the right limit");
near(clampSplit(0.5, 1200), 0.5, "inside the limits");
near(clampSplit(DEFAULT_SPLIT, 1200), DEFAULT_SPLIT, "the default is inside at 1200 px");
near(splitFromWidth(554 + 150, 1200), 704 / 1200, "a drag of +150 px");
near(splitFromWidth(1190, 1200) * 1200, 1200 - MIN_RIGHT, "a drag to the far right leaves the right column 420 px");

// A body too narrow for both limits (or unknown) has the default only.
eq(splitBounds(800).min, DEFAULT_SPLIT, "narrow body, min");
eq(splitBounds(800).max, DEFAULT_SPLIT, "narrow body, max");
eq(splitBounds(0).min, DEFAULT_SPLIT, "unknown body");
eq(clampSplit(0.7, 800), DEFAULT_SPLIT, "narrow body keeps the default");

// Keys.
near(splitAfterKey("ArrowRight", false, 0.5, 1200)!, 0.52, "ArrowRight is +2 %");
near(splitAfterKey("ArrowLeft", false, 0.5, 1200)!, 0.48, "ArrowLeft is -2 %");
near(splitAfterKey("ArrowRight", true, 0.5, 1200)!, 0.6, "Shift+ArrowRight is +10 %");
near(splitAfterKey("ArrowLeft", true, 0.5, 1200)!, 0.4, "Shift+ArrowLeft is -10 %");
near(splitAfterKey("ArrowLeft", true, 0.4, 1200)!, b.min, "keys stop at the left limit");
near(splitAfterKey("ArrowRight", true, 0.6, 1200)!, b.max, "keys stop at the right limit");
near(splitAfterKey("Home", false, 0.5, 1200)!, b.min, "Home");
near(splitAfterKey("End", false, 0.5, 1200)!, b.max, "End");
near(splitAfterKey("Enter", false, 0.5, 1200)!, DEFAULT_SPLIT, "Enter resets");
eq(splitAfterKey("a", false, 0.5, 1200), null, "other keys are not handled");

// The saved state.
const d = normalizeUi(undefined);
eq(d.split, DEFAULT_SPLIT, "no saved ui: default split");
eq(d.camera && d.controls && d.output, true, "no saved ui: panels open");
eq(d.advanced, false, "no saved ui: Advanced closed");
eq(JSON.stringify(normalizeUi(null)), JSON.stringify(DEFAULT_UI), "null is the default");
eq(JSON.stringify(normalizeUi("x")), JSON.stringify(DEFAULT_UI), "a string is the default");
eq(normalizeUi({ split: 0.6 }).split, 0.6, "a valid split is kept");
eq(normalizeUi({ split: "0.6" }).split, DEFAULT_SPLIT, "a string split is the default");
eq(normalizeUi({ split: NaN }).split, DEFAULT_SPLIT, "NaN is the default");
eq(normalizeUi({ split: 0 }).split, DEFAULT_SPLIT, "0 is the default");
eq(normalizeUi({ split: 1.5 }).split, DEFAULT_SPLIT, "1.5 is the default");
const u = normalizeUi({ camera: false, controls: false, output: false, advanced: true });
eq(u.camera || u.controls || u.output, false, "closed panels are kept");
eq(u.advanced, true, "an open Advanced is kept");
const junk = normalizeUi({ camera: "no", controls: 0, output: null, advanced: "yes" });
eq(junk.camera && junk.controls && junk.output, true, "non-boolean flags fall back to open");
eq(junk.advanced, false, "a non-boolean Advanced falls back to closed");

console.log("ok");
