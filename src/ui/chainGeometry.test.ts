/**
 * Checks of the pure part of the chain geometry: where the drop line goes for gap 0, in between, and gap n.
 * `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { gapLine } from "./chainGeometry";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

// A list padded 8 px at the top and bottom, in a block whose top is at 60: three 32 px rows from 108 to 204.
const rows = [
  { top: 108, bottom: 140 },
  { top: 140, bottom: 172 },
  { top: 172, bottom: 204 },
];
const listTop = 100;
const listBottom = 212;
const blockTop = 60;

eq(gapLine(rows, 0, listTop, listBottom, blockTop), 43, "gap 0 sits in the top padding: (100 + 108) / 2 - 60 - 1");
eq(gapLine(rows, 1, listTop, listBottom, blockTop), 79, "gap 1 is between row 1 and row 2: 140 - 60 - 1");
eq(gapLine(rows, 2, listTop, listBottom, blockTop), 111, "gap 2 is between row 2 and row 3: 172 - 60 - 1");
eq(gapLine(rows, 3, listTop, listBottom, blockTop), 147, "gap n sits in the bottom padding: (204 + 212) / 2 - 60 - 1");

// Rows with a gap between them: the line is in the middle of it.
const apart = [
  { top: 100, bottom: 130 },
  { top: 140, bottom: 170 },
];
eq(gapLine(apart, 1, 90, 180, 50), 84, "a line between rows 10 px apart is in the middle: 135 - 50 - 1");

// One row: gap 0 above it, gap 1 below it.
const one = [{ top: 108, bottom: 140 }];
eq(gapLine(one, 0, listTop, 148, blockTop), 43, "one row: gap 0");
eq(gapLine(one, 1, listTop, 148, blockTop), 83, "one row: gap 1 = (140 + 148) / 2 - 60 - 1");

// No row: the line is at the list's top.
eq(gapLine([], 0, listTop, listBottom, blockTop), 39, "no row: gap 0 is the list's top: 100 - 60 - 1");

// The result does not depend on where the block is scrolled to, only on the distances.
eq(gapLine(rows.map((r) => ({ top: r.top - 50, bottom: r.bottom - 50 })), 1, listTop - 50, listBottom - 50, blockTop - 50), 79, "scrolling the page changes nothing");

console.log("ok");
