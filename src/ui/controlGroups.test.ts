/**
 * Checks of the display groups of the camera controls. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { CONTROL_KEYS, type ControlKey } from "../camera/schema";
import { CONTROL_GROUPS, groupControls } from "./controlGroups";

function eq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

const of = (...keys: ControlKey[]) => keys.map((key) => ({ key }));
const shape = (infos: { key: ControlKey }[]) => groupControls(infos).map((g) => [g.title, g.items.map((i) => i.key)]);

// every key of the store is in exactly one group
const listed = CONTROL_GROUPS.flatMap((g) => g.keys);
eq([...listed].sort(), [...CONTROL_KEYS].sort(), "every control is in a group, once");

// nothing reported: nothing shown
eq(shape([]), [], "no controls");

// a group with no control the camera reports is left out
eq(shape(of("brightness", "contrast")), [["Picture", ["brightness", "contrast"]]], "only Picture");
eq(shape(of("zoom")), [["Lens", ["zoom"]]], "only Lens");

// groups keep their own order, whatever order the camera reported in
eq(
  shape(of("zoom", "brightness", "gain", "exposure", "focus", "contrast")),
  [
    ["Exposure and colour", ["exposure", "gain"]],
    ["Picture", ["brightness", "contrast"]],
    ["Lens", ["focus", "zoom"]],
  ],
  "group and row order",
);

// the whole store order lands in three groups and loses nothing
const all = groupControls(of(...CONTROL_KEYS));
eq(all.map((g) => g.title), ["Exposure and colour", "Picture", "Lens"], "three groups");
eq(all.flatMap((g) => g.items.map((i) => i.key)).sort(), [...CONTROL_KEYS].sort(), "nothing lost");

console.log("controlGroups: ok");
