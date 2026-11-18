/**
 * Checks of the pure logic behind the chain reorder (W1). `npm run test:ui` bundles and runs it (esbuild comes with vite).
 * It throws at the first failure and prints `ok` otherwise.
 */
import { EDGE_MAX_SPEED, EDGE_ZONE, edgeSpeed, finalIndex, gapFor, insertionIndex, keyboardGap, snapToSource } from "./chainDrag";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

// Four cards with midpoints at 100, 200, 300, 400.
const mids = [100, 200, 300, 400];
eq(insertionIndex(mids, -50), 0, "above the first card");
eq(insertionIndex(mids, 99), 0, "just above the first midpoint");
eq(insertionIndex(mids, 100), 0, "exactly on the first midpoint counts as above it");
eq(insertionIndex(mids, 101), 1, "just below the first midpoint");
eq(insertionIndex(mids, 250), 2, "between the second and third midpoints");
eq(insertionIndex(mids, 400), 3, "exactly on the last midpoint");
eq(insertionIndex(mids, 401), 4, "just below the last midpoint");
eq(insertionIndex(mids, 9999), 4, "below the last card");
eq(insertionIndex([], 10), 0, "no cards");
eq(insertionIndex([50], 49), 0, "one card, above");
eq(insertionIndex([50], 51), 1, "one card, below");

// Dropping into a gap.
eq(finalIndex(0, 3), 2, "card 1 into the gap above card 4 lands at step 3");
eq(finalIndex(2, 0), 0, "card 3 to the top");
eq(finalIndex(1, 1), 1, "the gap above the source stays");
eq(finalIndex(1, 2), 1, "the gap under the source stays");
eq(finalIndex(0, 4), 3, "card 1 to the very end");
eq(gapFor(0, 2), 3, "to land at index 2 from 0, the gap is above card index 3");
eq(gapFor(3, 1), 1, "to land at index 1 from 3, the gap is above card index 1");
for (let from = 0; from < 5; from++) {
  for (let to = 0; to < 5; to++) eq(finalIndex(from, gapFor(from, to)), to, `round trip ${from} to ${to}`);
}

eq(snapToSource(2, 2), 2, "gap above the source snaps to the source");
eq(snapToSource(2, 3), 2, "gap under the source snaps to the source");
eq(snapToSource(2, 1), 1, "a gap further up is kept");
eq(snapToSource(2, 4), 4, "a gap further down is kept");

// Auto-scroll: a 500 px tall container from y=100 to y=600.
eq(edgeSpeed(350, 100, 600), 0, "middle: no scroll");
eq(edgeSpeed(100 + EDGE_ZONE, 100, 600), 0, "on the zone border: no scroll");
eq(edgeSpeed(100, 100, 600), -EDGE_MAX_SPEED, "on the top edge: full speed up");
eq(edgeSpeed(-30, 100, 600), -EDGE_MAX_SPEED, "above the top edge: capped");
eq(edgeSpeed(600, 100, 600), EDGE_MAX_SPEED, "on the bottom edge: full speed down");
eq(edgeSpeed(700, 100, 600), EDGE_MAX_SPEED, "below the bottom edge: capped");
eq(edgeSpeed(100 + EDGE_ZONE / 2, 100, 600), -EDGE_MAX_SPEED / 2, "half way into the top zone: half speed");
eq(edgeSpeed(600 - EDGE_ZONE / 2, 100, 600), EDGE_MAX_SPEED / 2, "half way into the bottom zone: half speed");
if (!(edgeSpeed(120, 100, 600) < edgeSpeed(130, 100, 600) && edgeSpeed(130, 100, 600) < 0)) throw new Error("speed must grow towards the edge");

// Keyboard: card 1 of 3 lifted (from 0, gap 0).
eq(keyboardGap(0, 0, "ArrowDown", 3), 2, "down once: lands at step 2, gap above card index 2");
eq(finalIndex(0, keyboardGap(0, 0, "ArrowDown", 3)), 1, "down once from the top: index 1");
eq(finalIndex(0, keyboardGap(0, 0, "ArrowUp", 3)), 0, "up at the top stays");
eq(finalIndex(1, keyboardGap(1, 1, "ArrowUp", 3)), 0, "up from the middle");
eq(finalIndex(1, keyboardGap(1, 1, "ArrowDown", 3)), 2, "down from the middle");
eq(finalIndex(2, keyboardGap(2, 2, "ArrowDown", 3)), 2, "down at the bottom stays");
eq(finalIndex(1, keyboardGap(1, 1, "End", 3)), 2, "End");
eq(finalIndex(1, keyboardGap(1, 1, "Home", 3)), 0, "Home");
// Two presses down from index 1 of 4 (card 2 -> step 4), as in the CDP check.
const g1 = keyboardGap(1, 1, "ArrowDown", 4);
const g2 = keyboardGap(1, g1, "ArrowDown", 4);
eq(finalIndex(1, g2), 3, "two presses down from step 2 of 4 land at step 4");

console.log("ok");
