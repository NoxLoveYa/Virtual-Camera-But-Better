/**
 * The eyedropper (UX-PASS L2): the pixel of the preview under the pointer, the average of the 5×5 around it from the last frame's
 * RGBA bytes (no GL readback), and the OBS colour int for it. Pure: no DOM, no store.
 */
import { withRgb } from "../filters/values";
import { HEADER_BYTES, letterbox, type FrameHeader, type Rect } from "./previewFrame";

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
 * The average colour of the pixels within `radius` of (`x`, `y`) in the frame `buf` (header and RGBA rows, as the engine sends it),
 * as whole [r, g, b] bytes; the window is cut at the picture's edge. Alpha does not count: the preview is drawn opaque.
 */
export function sampleAverage(buf: ArrayBuffer, header: FrameHeader, x: number, y: number, radius = SAMPLE_RADIUS): [number, number, number] {
  const bytes = new Uint8Array(buf, HEADER_BYTES);
  const x0 = Math.max(0, x - radius);
  const x1 = Math.min(header.width - 1, x + radius);
  const y0 = Math.max(0, y - radius);
  const y1 = Math.min(header.height - 1, y + radius);
  const sum = [0, 0, 0];
  let n = 0;
  for (let j = y0; j <= y1; j++) {
    for (let i = x0; i <= x1; i++) {
      const at = j * header.stride + i * 4;
      sum[0] += bytes[at];
      sum[1] += bytes[at + 1];
      sum[2] += bytes[at + 2];
      n++;
    }
  }
  return n === 0 ? [0, 0, 0] : [Math.round(sum[0] / n), Math.round(sum[1] / n), Math.round(sum[2] / n)];
}

/** The key colour for a picked [r, g, b]: OBS `0xAABBGGRR` with the alpha byte of `current` kept. */
export const keyColorOf = (current: number, rgb: readonly [number, number, number]): number => withRgb(current, rgb[0], rgb[1], rgb[2]);
