/**
 * The eyedropper (UX-PASS L2): the pixel of the preview under the pointer, the average of the 5×5 around it from the last frame's
 * NV12 bytes (no GL readback: each sampled pixel is converted to RGB here with the matrix of `yuv.ts`, the one the shader uses),
 * and the OBS colour int for it. Pure: no DOM, no store.
 *
 * Why JS and not a readback of the canvas: `gl.readPixels` stalls the page on the GPU and needs the drawing buffer kept, and the
 * pick would then depend on when the canvas was last drawn. 25 pixels converted from the frame the page already holds are
 * exact for the same matrix (the shader differs only in that it interpolates chroma between texels, which a flat patch and a
 * 25 pixel average do not show: `eyedropper.test.ts` pins it within 2 codes).
 */
import { withRgb } from "../filters/values";
import { HEADER_BYTES, chromaOffset, letterbox, type FrameHeader, type Rect } from "./previewFrame";
import { nv12ToRgb } from "./yuv";

/** The window around the pointer the eyedropper averages: 5×5 preview pixels. */
export const SAMPLE_RADIUS = 2;

/**
 * The preview pixel under (`clientX`, `clientY`) in a canvas of `canvas` size drawn letterboxed in `box` (the preview box's rect), or
 * `null` in the letterbox bars.
 */
export function toCanvasPixel(clientX: number, clientY: number, box: Rect, canvas: { width: number; height: number }): { x: number; y: number } | null {
  const { s, ox, oy } = letterbox(box, canvas);
  const x = Math.floor((clientX - box.left - ox) / s);
  const y = Math.floor((clientY - box.top - oy) / s);
  return x >= 0 && x < canvas.width && y >= 0 && y < canvas.height ? { x, y } : null;
}

/**
 * The average colour of the pixels within `radius` of (`x`, `y`) in the frame `buf` (header, Y plane and UV plane, as the engine
 * sends it), as whole [r, g, b] bytes: each pixel is converted to RGB first (its Y and the Cb, Cr of its 2×2 block), then the
 * RGB values are averaged. The window is cut at the picture's edge.
 */
export function sampleAverage(buf: ArrayBuffer, header: FrameHeader, x: number, y: number, radius = SAMPLE_RADIUS): [number, number, number] {
  const bytes = new Uint8Array(buf);
  const uv = chromaOffset(header);
  const x0 = Math.max(0, x - radius);
  const x1 = Math.min(header.width - 1, x + radius);
  const y0 = Math.max(0, y - radius);
  const y1 = Math.min(header.height - 1, y + radius);
  const sum = [0, 0, 0];
  let n = 0;
  for (let j = y0; j <= y1; j++) {
    const row = uv + (j >> 1) * header.stride;
    for (let i = x0; i <= x1; i++) {
      const c = row + (i >> 1) * 2;
      const rgb = nv12ToRgb(bytes[HEADER_BYTES + j * header.stride + i], bytes[c], bytes[c + 1]);
      sum[0] += rgb[0];
      sum[1] += rgb[1];
      sum[2] += rgb[2];
      n++;
    }
  }
  return n === 0 ? [0, 0, 0] : [Math.round(sum[0] / n), Math.round(sum[1] / n), Math.round(sum[2] / n)];
}

/** The key colour for a picked [r, g, b]: OBS `0xAABBGGRR` with the alpha byte of `current` kept. */
export const keyColorOf = (current: number, rgb: readonly [number, number, number]): number => withRgb(current, rgb[0], rgb[1], rgb[2]);
