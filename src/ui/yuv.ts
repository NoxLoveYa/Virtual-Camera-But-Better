/**
 * The preview's colour conversion (NV12 to RGB), in one place for the two readers of a frame: the WebGL fragment shader of
 * `Preview.tsx` (which gets the GLSL below) and the eyedropper (which converts the few pixels it samples in JS).
 *
 * The matrix is BT.709, limited (studio) range, 8 bit: exactly what the engine encodes (`gpu::output::dispatch_packed_nv12`
 * uses `rgb_to_yuv_rows(Bt709, Limited)`, the media type the virtual camera advertises) and the inverse of
 * `gpu::convert::yuv_to_rgb_rows(Bt709, Limited, 8)`. Y is 16 to 235 and Cb, Cr are 16 to 240 around 128; the rows are the
 * same ones the Rust test `the_page_matrix_is_the_inverse_of_the_encoders` pins. Pure: no DOM, no store.
 *
 *   R = 1.164384 Y' + 1.792741 Cr' - 0.972945
 *   G = 1.164384 Y' - 0.213249 Cb' - 0.532909 Cr' + 0.301483
 *   B = 1.164384 Y' + 2.112402 Cb' - 1.133402
 *
 * with Y', Cb', Cr' the bytes divided by 255.
 */

/** BT.709 luma weights. */
const KR = 0.2126;
const KB = 0.0722;

/** Rows `[y, cb, cr, 1]` of the matrix from normalised (byte / 255) samples to normalised RGB, derived like `yuv_to_rgb_rows`. */
export function yuvToRgbRows(): [number[], number[], number[]] {
  const kg = 1 - KR - KB;
  // limited range, 8 bit: luma 16 + 219 x, chroma 128 + 224 x (all over 255)
  const ySpan = 219 / 255;
  const yOff = 16 / 255;
  const cSpan = 224 / 255;
  const cZero = 128 / 255;
  const cr = 2 * (1 - KR);
  const cb = 2 * (1 - KB);
  const gPb = (-cb * KB) / kg;
  const gPr = (-cr * KR) / kg;
  const ay = 1 / ySpan;
  const a = 1 / cSpan;
  const oy = -yOff / ySpan;
  const oc = -cZero / cSpan;
  return [
    [ay, 0, cr * a, oy + cr * oc],
    [ay, gPb * a, gPr * a, oy + gPb * oc + gPr * oc],
    [ay, cb * a, 0, oy + cb * oc],
  ];
}

const ROWS = yuvToRgbRows();

/** One NV12 pixel as [r, g, b], 0 to 255, clamped and not rounded: the bytes `y`, `cb`, `cr` of the two planes. */
export function nv12ToRgb(y: number, cb: number, cr: number): [number, number, number] {
  const v = [y / 255, cb / 255, cr / 255];
  return ROWS.map((r) => Math.min(Math.max(r[0] * v[0] + r[1] * v[1] + r[2] * v[2] + r[3], 0), 1) * 255) as [number, number, number];
}

/** The inverse, for tests: [r, g, b] bytes (0 to 255) to the bytes [y, cb, cr] as the engine's encoder writes them (rounded). */
export function rgbToNv12(r: number, g: number, b: number): [number, number, number] {
  const kg = 1 - KR - KB;
  const [rn, gn, bn] = [r / 255, g / 255, b / 255];
  const y = KR * rn + kg * gn + KB * bn;
  const cb = (bn - y) / (2 * (1 - KB));
  const cr = (rn - y) / (2 * (1 - KR));
  const byte = (v: number) => Math.round(Math.min(Math.max(v, 0), 1) * 255);
  return [byte((16 + 219 * y) / 255), byte((128 + 224 * cb) / 255), byte((128 + 224 * cr) / 255)];
}

const glsl = (row: number[]) => `vec4(${row.map((v) => v.toFixed(7)).join(", ")})`;

/**
 * The GLSL ES 3.00 function `vec3 yuvToRgb(float y, vec2 c)` of the same matrix: `y` and `c` (Cb, Cr) are the sampled
 * `R8` and `RG8` texels (byte / 255); the result is the RGB colour, clamped to 0..1.
 */
export const YUV_TO_RGB_GLSL = `vec3 yuvToRgb(float y, vec2 c) {
  vec4 v = vec4(y, c, 1.0);
  return clamp(vec3(dot(${glsl(ROWS[0])}, v), dot(${glsl(ROWS[1])}, v), dot(${glsl(ROWS[2])}, v)), 0.0, 1.0);
}`;
