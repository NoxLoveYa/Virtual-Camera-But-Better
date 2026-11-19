/**
 * Checks of the eyedropper and the preview tools' pure parts: the preview pixel under the pointer, the 5×5 average, the OBS colour
 * packing, which filters have a tool, why one is blocked, and the source-size watch. `npm run test:ui` bundles and runs it.
 */
import { colorAlpha, colorToHex } from "../filters/values";
import { keyColorOf, sampleAverage, toCanvasPixel } from "./eyedropper";
import { HEADER_BYTES, toSourceEdge, type FrameHeader } from "./previewFrame";
import { SETTLE_MS, sourceChange, toolBlock, toolOf } from "./previewTool";

function eq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

// A 10x8 picture with a 48-byte stride (8 bytes of padding per row); each pixel's colour says where it is: r = x, g = y, b = 100.
const header: FrameHeader = { width: 10, height: 8, stride: 48, frame: 1, sourceWidth: 1000, sourceHeight: 800 };
const buf = new ArrayBuffer(HEADER_BYTES + header.stride * header.height);
{
  const bytes = new Uint8Array(buf, HEADER_BYTES);
  for (let y = 0; y < header.height; y++) {
    for (let x = 0; x < header.width; x++) bytes.set([x * 10, y * 10, 100, 255], y * header.stride + x * 4);
    bytes.fill(7, y * header.stride + 40, (y + 1) * header.stride); // the padding must never be read
  }
}

// ---- the pixel under the pointer: a 10x8 canvas in a 200x80 box at (50, 20) is pillarboxed at scale 10: 100 px wide, offset 50
const box = { left: 50, top: 20, width: 200, height: 80 };
eq(toCanvasPixel(50 + 50, 20, box, header), { x: 0, y: 0 }, "the top left pixel");
eq(toCanvasPixel(50 + 50 + 99, 20 + 79, box, header), { x: 9, y: 7 }, "the bottom right pixel");
eq(toCanvasPixel(50 + 50 + 35, 20 + 25, box, header), { x: 3, y: 2 }, "a pixel in the middle");
eq(toCanvasPixel(50 + 49, 20 + 10, box, header), null, "the left bar");
eq(toCanvasPixel(50 + 150, 20 + 10, box, header), null, "the right bar");
eq(toCanvasPixel(50 + 100, 20 - 1, box, header), null, "above the box");

// ---- the 5x5 average
eq(sampleAverage(buf, header, 5, 4), [50, 40, 100], "the middle: the average of x 3..7 and y 2..6");
eq(sampleAverage(buf, header, 0, 0), [10, 10, 100], "the corner is cut at the edge (x 0..2, y 0..2)");
eq(sampleAverage(buf, header, 9, 7), [80, 60, 100], "the far corner");
eq(sampleAverage(buf, header, 5, 4, 0), [50, 40, 100], "radius 0 is the pixel");
eq(sampleAverage(buf, header, 1, 1, 100), [45, 35, 100], "a huge radius is the whole picture");

// ---- OBS 0xAABBGGRR, alpha kept
const picked = keyColorOf(0x80112233, [255, 0, 128]);
eq(picked, 0x808000ff, "alpha byte kept, blue and red in place");
eq(colorAlpha(picked), 0x80, "the alpha is the old one");
eq(colorToHex(picked), "#ff0080", "the hex reads red, green, blue");
eq(keyColorOf(0x0000ff00, [1, 2, 3]), 0x00030201, "an alpha of 0 stays 0");
eq(keyColorOf(0xff000000, [255, 255, 255]), 0xffffffff, "the result is an unsigned number");
eq(keyColorOf(0, [300, -5, 12.6]), 0x000d00ff, "out-of-range parts are clamped");

// ---- the unrounded source point
eq(toSourceEdge(50 + 50, 20, box, header, { width: 1000, height: 800 }), { x: 0, y: 0 }, "the top left edge");
eq(toSourceEdge(50 + 150, 20 + 80, box, header, { width: 1000, height: 800 }), { x: 1000, y: 800 }, "the far corner is the source size, not a pixel index");
eq(toSourceEdge(0, 0, box, header, { width: 1000, height: 800 }), { x: 0, y: 0 }, "outside clamps to the edge");
eq(toSourceEdge(50 + 50 + 50, 20 + 40, box, header, { width: 1000, height: 800 }), { x: 500, y: 400 }, "the middle");

// ---- which filters have a tool
eq(toolOf("chroma_key_filter"), "pick", "chroma key");
eq(toolOf("color_key_filter"), "pick", "color key");
eq(toolOf("crop_filter"), "crop", "crop/pad");
eq(toolOf("luma_key_filter"), null, "luma key has none");
eq(toolOf("ext:acme/thing"), null, "an extension filter has none");

// ---- why a tool is blocked
const ok = { previewOn: true, state: "live" as const, paused: false };
eq(toolBlock(ok), null, "live and shown: free");
eq(toolBlock({ ...ok, previewOn: false }) !== null, true, "preview off");
eq(toolBlock({ ...ok, state: "stopped" }) !== null, true, "engine stopped");
eq(toolBlock({ ...ok, state: "standby" }) !== null, true, "camera off or opening");
eq(toolBlock({ ...ok, state: "reconnecting" }) !== null, true, "camera lost");
eq(toolBlock({ ...ok, paused: true }) !== null, true, "paused");
eq(toolBlock({ ...ok, previewOn: false, state: "stopped" }), toolBlock({ ...ok, previewOn: false }), "preview off is said first");

// ---- the source-size watch
const a = { w: 1280, h: 720 };
const b = { w: 640, h: 360 };
eq(sourceChange(null, a, 1000, 5000), { next: a, changed: false }, "the first frame is the baseline");
eq(sourceChange(a, a, 1000, 5000), { next: a, changed: false }, "the same size");
eq(sourceChange(a, b, 1000, 1000 + SETTLE_MS - 1), { next: b, changed: false }, "inside the settle window another size moves the baseline");
eq(sourceChange(a, b, 1000, 1000 + SETTLE_MS), { next: a, changed: true }, "after it another size is a change");
eq(sourceChange(a, { w: 1280, h: 721 }, 1000, 9000).changed, true, "a height change counts");
console.log("eyedropper: ok");
