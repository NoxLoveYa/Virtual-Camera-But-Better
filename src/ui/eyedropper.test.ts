/**
 * Checks of the eyedropper and the preview tools' pure parts: the preview pixel under the pointer, the 5×5 average, the OBS colour
 * packing, which filters have a tool, why one is blocked, and the source-size watch. `npm run test:ui` bundles and runs it.
 */
import { colorAlpha, colorToHex } from "../filters/values";
import { keyColorOf, sampleAverage, toCanvasPixel } from "./eyedropper";
import { HEADER_BYTES, chromaOffset, toSourceEdge, type FrameHeader } from "./previewFrame";
import { SETTLE_MS, sourceChange, toolBlock, toolOf } from "./previewTool";
import { YUV_TO_RGB_GLSL, nv12ToRgb, rgbToNv12, yuvToRgbRows } from "./yuv";

function eq<T>(actual: T, expected: T, what: string) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

/** `actual` is within `tol` codes of `expected`, channel by channel. */
function near(actual: readonly number[], expected: readonly number[], tol: number, what: string) {
  if (actual.length !== expected.length || actual.some((v, i) => Math.abs(v - expected[i]) > tol)) {
    throw new Error(`${what}: expected ${JSON.stringify(expected)} within ${tol}, got ${JSON.stringify(actual)}`);
  }
}

// ---- the colour matrix: BT.709, limited range, the one the engine encodes (the Rust test pins the same rows)
const rows = yuvToRgbRows();
const expectedRows = [
  [1.164384, 0, 1.792741, -0.972945],
  [1.164384, -0.213249, -0.532909, 0.301483],
  [1.164384, 2.112402, 0, -1.133402],
];
rows.forEach((row, i) => near(row, expectedRows[i], 0.00001, `row ${i} of the matrix`));
near(nv12ToRgb(16, 128, 128), [0, 0, 0], 0.01, "limited black");
near(nv12ToRgb(235, 128, 128), [255, 255, 255], 0.01, "limited white");
near(nv12ToRgb(126, 128, 128), [128.2, 128.2, 128.2], 0.5, "mid grey is neutral");
// the textbook BT.709 limited values of the primaries and the secondaries (Y, Cb, Cr)
near(nv12ToRgb(63, 102, 240), [255, 0, 0], 1.5, "red");
near(nv12ToRgb(173, 42, 26), [0, 255, 0], 1.5, "green");
near(nv12ToRgb(32, 240, 118), [0, 0, 255], 1.5, "blue");
near(nv12ToRgb(219, 16, 138), [255, 255, 0], 1.5, "yellow");
eq(nv12ToRgb(0, 128, 128).every((v) => v === 0), true, "a luma below the range clamps to 0");
eq(nv12ToRgb(255, 128, 128).every((v) => v === 255), true, "a luma above the range clamps to 255");
eq(nv12ToRgb(0, 0, 0).every((v) => v >= 0 && v <= 255), true, "out of range chroma stays in 0..255");
eq(nv12ToRgb(255, 255, 255).every((v) => v >= 0 && v <= 255), true, "out of range chroma stays in 0..255 (high)");
// the encoder and its inverse agree on every primary and a coarse cube, to the rounding of three 8 bit samples
{
  let worst = 0;
  for (let r = 0; r <= 255; r += 15) {
    for (let g = 0; g <= 255; g += 15) {
      for (let b = 0; b <= 255; b += 15) {
        const [y, cb, cr] = rgbToNv12(r, g, b);
        const back = nv12ToRgb(y, cb, cr);
        worst = Math.max(worst, Math.abs(back[0] - r), Math.abs(back[1] - g), Math.abs(back[2] - b));
      }
    }
  }
  // one code of Cr is 1.79 / 255 x 255 = up to 2.1 codes of red, one of Cb 2.1 of blue; rounding is half of that
  eq(worst <= 3.2, true, `rgb -> nv12 -> rgb is within about 3 codes (worst ${worst.toFixed(2)})`);
}
// the shader carries the same numbers
{
  const inGlsl = [...YUV_TO_RGB_GLSL.matchAll(/vec4\(([-\d., ]+)\)/g)].map((m) => m[1].split(",").map(Number)).filter((r) => r.length === 4 && r[3] !== 1);
  eq(inGlsl.length, 3, "three matrix rows in the GLSL");
  inGlsl.forEach((row, i) => near(row, expectedRows[i], 0.00001, `row ${i} in the GLSL`));
}
eq(YUV_TO_RGB_GLSL.startsWith("vec3 yuvToRgb(float y, vec2 c)"), true, "the GLSL function the fragment shader calls");

// A 10x8 NV12 picture with a 48-byte stride (stride is 38 bytes wider than the row); each pixel's colour says where it is:
// r = 10 x, g = 10 y, b = 100. Chroma is made from the mean colour of each 2x2 block, as the engine's encoder does. The
// padding after every row of both planes is garbage that must never be read.
const header: FrameHeader = { width: 10, height: 8, stride: 48, frame: 1, sourceWidth: 1000, sourceHeight: 800 };
const buf = new ArrayBuffer(HEADER_BYTES + (header.stride * header.height * 3) / 2);
{
  const bytes = new Uint8Array(buf);
  bytes.fill(7, HEADER_BYTES);
  const colorAt = (x: number, y: number) => [x * 10, y * 10, 100];
  for (let y = 0; y < header.height; y++) {
    for (let x = 0; x < header.width; x++) bytes[HEADER_BYTES + y * header.stride + x] = rgbToNv12(...(colorAt(x, y) as [number, number, number]))[0];
  }
  for (let by = 0; by < header.height / 2; by++) {
    for (let bx = 0; bx < header.width / 2; bx++) {
      const mean = [0, 1, 2].map((c) => [0, 1, 2, 3].reduce((s, k) => s + colorAt(bx * 2 + (k & 1), by * 2 + (k >> 1))[c], 0) / 4);
      const [, cb, cr] = rgbToNv12(mean[0], mean[1], mean[2]);
      bytes.set([cb, cr], chromaOffset(header) + by * header.stride + bx * 2);
    }
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

// ---- the 5x5 average, from NV12 (the 8 bit round trip and the chroma shared by 2x2 pixels cost a few codes on this gradient)
near(sampleAverage(buf, header, 5, 4), [50, 40, 100], 3, "the middle: the average of x 3..7 and y 2..6");
near(sampleAverage(buf, header, 0, 0), [10, 10, 100], 3, "the corner is cut at the edge (x 0..2, y 0..2)");
near(sampleAverage(buf, header, 9, 7), [80, 60, 100], 3, "the far corner");
// a single pixel carries the chroma of its 2x2 block (the mean of x 4..5, y 4..5 here: r 45, g 45), so it can be off by half a step
near(sampleAverage(buf, header, 5, 4, 0), [50, 40, 100], 8, "radius 0 is the pixel");
near(sampleAverage(buf, header, 1, 1, 100), [45, 35, 100], 3, "a huge radius is the whole picture");
// a flat patch comes back within 2 codes (the brief: the eyedropper keeps its colours), for a spread of colours
for (const color of [[255, 0, 0], [0, 255, 0], [0, 0, 255], [200, 30, 90], [20, 120, 220], [128, 128, 128], [250, 250, 250], [4, 4, 4]] as const) {
  const flat: FrameHeader = { width: 8, height: 8, stride: 8, frame: 2, sourceWidth: 8, sourceHeight: 8 };
  const flatBuf = new ArrayBuffer(HEADER_BYTES + 96);
  const flatBytes = new Uint8Array(flatBuf);
  const [y, cb, cr] = rgbToNv12(color[0], color[1], color[2]);
  flatBytes.fill(y, HEADER_BYTES, HEADER_BYTES + 64);
  for (let i = 0; i < 32; i += 2) flatBytes.set([cb, cr], chromaOffset(flat) + i);
  near(sampleAverage(flatBuf, flat, 4, 4), color, 2.6, `a flat ${color}`);
}
// the padding after the rows is never read: garbage there (the fixture's 7s) did not move the averages above, and a window
// over the right edge of the picture takes only the real columns
near(sampleAverage(buf, header, 9, 4, 2), [80, 40, 100], 3, "the window at the right edge: x 7..9");

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
