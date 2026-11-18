/**
 * Checks of the pure logic behind inserting a filter at a position (L5). `npm run test:ui` bundles and runs it.
 * It throws at the first failure and prints `ok` otherwise.
 */
import { DRAG_THRESHOLD } from "./chainDrag";
import { CHAIN_MARGIN, classifyPress, gapLabel, overChain } from "./insertDrag";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

const small = DRAG_THRESHOLD - 1;
const big = DRAG_THRESHOLD + 2;

// A press is a click until it moves a few pixels.
eq(classifyPress("mouse", 0, 0), "wait", "no movement");
eq(classifyPress("mouse", small, 0), "wait", "mouse below the threshold");
eq(classifyPress("mouse", 0, big), "drag", "mouse straight down");
eq(classifyPress("mouse", -big, 0), "drag", "mouse sideways");
eq(classifyPress("pen", 0, big), "drag", "a pen drags like a mouse");
// A finger on the vertical menu: sideways drags, vertical scrolls the list.
eq(classifyPress("touch", 0, big), "scroll", "touch vertical scrolls the list");
eq(classifyPress("touch", -big, 1), "drag", "touch sideways drags");
eq(classifyPress("touch", big, big), "scroll", "touch on the diagonal counts as a scroll");
eq(classifyPress("touch", small, 0), "wait", "touch below the threshold");

// The chain block is x 100…500, y 200…600; the column shows y 150…650.
const block = { left: 100, right: 500, top: 200, bottom: 600 };
const column = { left: 80, right: 520, top: 150, bottom: 650 };
eq(overChain(300, 400, block, column), true, "inside the block");
eq(overChain(99, 400, block, column), false, "left of the block");
eq(overChain(501, 400, block, column), false, "right of the block");
eq(overChain(300, 160, block, column), true, "above the block, exactly at the margin");
eq(overChain(300, 159, block, column), false, "above the margin");
eq(overChain(300, 190, block, { ...column, top: 195 }), false, "above the column's top edge");
eq(overChain(300, 640, block, column), true, "below the block, inside the margin and the column");
eq(overChain(300, 651, block, column), false, "below the column");
// A block scrolled far above the column: the margin does not reach the column.
eq(overChain(300, 300, { ...block, top: -1000, bottom: 100 }, column), false, "the chain scrolled out of view");
eq(overChain(300, 600 + CHAIN_MARGIN, block, { ...column, bottom: 900 }), true, "exactly at the margin");
eq(overChain(300, 600 + CHAIN_MARGIN + 1, block, { ...column, bottom: 900 }), false, "just past the margin");

eq(gapLabel(2), "Add a filter between step 2 and 3", "label of the gap after step 2");
eq(gapLabel(1), "Add a filter between step 1 and 2", "label of the gap after step 1");

console.log("ok");
