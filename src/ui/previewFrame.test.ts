/**
 * Checks of the preview helpers: the 28-byte frame header, the preview point and its caption, hold to compare, and the
 * mapping from the canvas to the tapped picture. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { FORMAT_NV12, HEADER_BYTES, NOT_HELD, chromaOffset, holdStep, isHeld, letterbox, pointFilter, pointLabel, readHeader, samePoint, toSourcePixel, type Hold, type HoldEvent } from "./previewFrame";

function eq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

/** A frame as `preview.rs` packs it: seven little-endian words (the last is the format), then the Y plane and the UV plane. */
function frame(words: number[], pixelBytes: number, format = FORMAT_NV12): ArrayBuffer {
  const buf = new ArrayBuffer(HEADER_BYTES + pixelBytes);
  const v = new DataView(buf);
  [...words, format].forEach((w, i) => v.setUint32(i * 4, w, true));
  return buf;
}

// ---- the header (NV12: `stride * height` bytes of Y, then `stride * height / 2` of UV)
eq(HEADER_BYTES, 28, "header size");
eq(FORMAT_NV12, 1, "the format word of NV12");
eq(readHeader(frame([4, 2, 4, 9, 1920, 1080], 12)), { width: 4, height: 2, stride: 4, frame: 9, sourceWidth: 1920, sourceHeight: 1080 }, "a good frame");
eq(readHeader(frame([4, 2, 6, 9, 8, 4], 18)), { width: 4, height: 2, stride: 6, frame: 9, sourceWidth: 8, sourceHeight: 4 }, "a stride wider than the row");
eq(chromaOffset({ width: 4, height: 2, stride: 6, frame: 0, sourceWidth: 1, sourceHeight: 1 }), HEADER_BYTES + 12, "the UV plane starts after the Y rows");
eq(readHeader(frame([4, 2, 4, 9, 1920, 1080], 11)), null, "pixels cut short (the UV plane is part of the pixels)");
eq(readHeader(frame([4, 2, 4, 9, 1920, 1080], 8)), null, "a Y plane without its UV plane");
eq(readHeader(frame([4, 2, 4, 9, 1920, 1080], 0).slice(0, 27)), null, "shorter than the header");
eq(readHeader(new ArrayBuffer(24)), null, "an old 24-byte header is not a frame");
eq(readHeader(frame([4, 2, 16, 9, 1920, 1080], 48, 0)), null, "an RGBA frame (no format word) is not read as NV12");
eq(readHeader(frame([4, 2, 4, 9, 1920, 1080], 12, 2)), null, "an unknown format");
eq(readHeader(frame([0, 2, 4, 9, 1920, 1080], 12)), null, "zero width");
eq(readHeader(frame([3, 2, 4, 9, 1920, 1080], 12)), null, "an odd width has no whole chroma block");
eq(readHeader(frame([4, 3, 4, 9, 1920, 1080], 18)), null, "an odd height has no whole chroma block");
eq(readHeader(frame([4, 2, 2, 9, 1920, 1080], 12)), null, "stride under a row");
eq(readHeader(frame([4, 2, 5, 9, 1920, 1080], 15)), null, "stride not even (the UV rows are read as pairs)");
eq(readHeader(frame([4, 2, 4, 9, 0, 1080], 12)), null, "no source width");
eq(readHeader(frame([4, 2, 4, 9, 1920, 0], 12)), null, "no source height");
eq(readHeader(frame([0xffffffff, 2, 0xffffffff, 9, 1, 1], 12)), null, "huge numbers do not overflow into a pass");

// ---- the point and its caption
const steps = [
  { id: "a", label: "Color Correction" },
  { id: "b", label: "Crop/Pad" },
];
eq(samePoint(null, null), true, "null is null");
eq(samePoint("input", "input"), true, "input is input");
eq(samePoint("input", null), false, "input is not null");
eq(samePoint({ filter: "a" }, { filter: "a" }), true, "same filter");
eq(samePoint({ filter: "a" }, { filter: "b" }), false, "other filter");
eq(samePoint({ filter: "a" }, "input"), false, "filter is not input");
eq(pointFilter({ filter: "b" }), "b", "the filter of a filter point");
eq(pointFilter("input"), null, "input names no filter");
eq(pointFilter(null), null, "the output names no filter");
eq(pointLabel(null, steps), null, "no caption for the output");
eq(pointLabel("input", steps), "Before", "the compare caption");
eq(pointLabel({ filter: "b" }, steps), "Crop/Pad · step 2 · showing the picture before this filter", "a filter caption");
eq(pointLabel({ filter: "gone" }, steps), null, "a filter that left the chain has no caption");

// ---- hold to compare
const run = (events: HoldEvent[]): Hold => events.reduce(holdStep, NOT_HELD);
eq(isHeld(NOT_HELD), false, "not held at first");
eq(isHeld(run(["pointerdown"])), true, "held by the pointer");
eq(isHeld(run(["pointerdown", "pointerup"])), false, "released by the pointer");
eq(isHeld(run(["keydown"])), true, "held by the key");
eq(isHeld(run(["keydown", "keydown", "keydown"])), true, "key repeat stays held");
eq(isHeld(run(["keydown", "keydown", "keyup"])), false, "one key up lets go after repeats");
eq(isHeld(run(["pointerdown", "keydown", "keyup"])), true, "still held by the pointer");
eq(isHeld(run(["pointerdown", "keydown", "pointerup"])), true, "still held by the key");
eq(isHeld(run(["pointerdown", "keydown", "pointerup", "keyup"])), false, "both let go");
eq(isHeld(run(["pointerdown", "keydown", "blur"])), false, "a blur lets go of both");
eq(isHeld(run(["pointerup", "keyup", "blur"])), false, "releases without a press change nothing");
eq(holdStep(NOT_HELD, "pointerup"), NOT_HELD, "the same state comes back when nothing changed");

// ---- from the canvas to the tapped picture
// A 16:9 box 640x360 at (100, 50), a 4:3 canvas 320x240 in it (pillarboxed), the source 1280x960.
const box = { left: 100, top: 50, width: 640, height: 360 };
const canvas = { width: 320, height: 240 };
const source = { width: 1280, height: 960 };
const lb = letterbox(box, canvas);
eq(lb, { s: 1.5, ox: 80, oy: 0 }, "the letterbox of a 4:3 canvas in 16:9");
eq(toSourcePixel(100 + 80, 50, box, canvas, source), { x: 0, y: 0 }, "the top left corner of the picture");
eq(toSourcePixel(100 + 80 + 240, 50 + 180, box, canvas, source), { x: 640, y: 480 }, "the middle of the picture");
eq(toSourcePixel(100 + 80 + 479, 50 + 359, box, canvas, source), { x: 1277, y: 957 }, "the bottom right pixel");
eq(toSourcePixel(100 + 79, 50 + 100, box, canvas, source), null, "the left bar is outside the picture");
eq(toSourcePixel(100 + 80 + 480, 50 + 100, box, canvas, source), null, "the right bar is outside the picture");
eq(toSourcePixel(100 + 79, 50 + 100, box, canvas, source, true), { x: 0, y: 267 }, "a drag in the bar clamps to the edge");
eq(toSourcePixel(100 + 700, 50 + 900, box, canvas, source, true), { x: 1279, y: 959 }, "a drag far outside clamps to the last pixel");
// the source is not larger than the canvas (a small picture is never upscaled): the mapping is the identity at scale 1
eq(toSourcePixel(10 + 5, 20 + 7, { left: 10, top: 20, width: 100, height: 100 }, { width: 100, height: 100 }, { width: 100, height: 100 }), { x: 5, y: 7 }, "one to one");
console.log("previewFrame: ok");
