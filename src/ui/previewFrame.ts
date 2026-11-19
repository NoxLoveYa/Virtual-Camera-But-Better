/**
 * The preview's wire format and what rides on it (UX-PASS L1, L4): the frame header of `src-tauri/src/preview.rs`, the
 * preview point (which picture the preview shows while a gesture runs), the label for it, the hold-to-compare state and
 * the mapping from a point on the canvas back to a pixel of the tapped picture (the eyedropper and the crop handles).
 * Pure: no DOM, no store.
 */

/**
 * Seven little-endian `u32`: width, height, stride, frame, source width, source height, format. Then the pixels. A picture is
 * `FORMAT_NV12` (BT.709 limited range, see `yuv.ts`): the Y plane, `height` rows of `stride` bytes, then the UV plane,
 * `height / 2` rows of `stride` bytes (`width / 2` interleaved Cb, Cr pairs). `FORMAT_NV12_PAIR` (the split-wipe Compare) is
 * two such pictures of the header's size one after the other: the camera picture before the chain, then the output.
 */
export const HEADER_BYTES = 28;

/** The `format` word of an NV12 frame. */
export const FORMAT_NV12 = 1;

/** The `format` word of a Compare frame: two NV12 pictures, the camera picture first, the output second. */
export const FORMAT_NV12_PAIR = 2;

export interface FrameHeader {
  width: number;
  height: number;
  /** Bytes per row of the Y plane and of the UV plane. */
  stride: number;
  frame: number;
  /** The full size of the tapped picture before the downscale: what a point on the canvas maps back to. */
  sourceWidth: number;
  sourceHeight: number;
  /** Present (true) on a Compare frame: two pictures, see `pictureOffset`. */
  pair?: true;
}

/** Bytes of one NV12 picture of the frame: the Y plane and the half height UV plane. */
export const pictureBytes = (h: FrameHeader): number => (h.stride * h.height * 3) / 2;

/** Where picture `index` of the frame (0, or 1 on a Compare frame: the output) starts in the buffer, after the header. */
export const pictureOffset = (h: FrameHeader, index: 0 | 1 = 0): number => HEADER_BYTES + index * pictureBytes(h);

/** Where the UV plane of picture `index` starts in a frame buffer (after the header and its Y plane). */
export const chromaOffset = (h: FrameHeader, index: 0 | 1 = 0): number => pictureOffset(h, index) + h.stride * h.height;

/** The header of a frame, or `null` when the frame is shorter than its header says, a field is impossible or the format is not NV12 (one picture or a pair). */
export function readHeader(buf: ArrayBuffer): FrameHeader | null {
  if (buf.byteLength < HEADER_BYTES) return null;
  const v = new DataView(buf);
  const [width, height, stride, frame, sourceWidth, sourceHeight, format] = [0, 4, 8, 12, 16, 20, 24].map((at) => v.getUint32(at, true));
  const pictures = format === FORMAT_NV12 ? 1 : format === FORMAT_NV12_PAIR ? 2 : 0;
  // 4:2:0 needs whole 2 x 2 blocks, and the chroma rows are read as `stride / 2` texels of 2 bytes
  const complete =
    pictures > 0 &&
    width > 0 &&
    height > 0 &&
    width % 2 === 0 &&
    height % 2 === 0 &&
    sourceWidth > 0 &&
    sourceHeight > 0 &&
    stride >= width &&
    stride % 2 === 0 &&
    buf.byteLength >= HEADER_BYTES + pictures * ((stride * height * 3) / 2);
  return complete ? { width, height, stride, frame, sourceWidth, sourceHeight, ...(pictures === 2 && { pair: true as const }) } : null;
}

/**
 * What `set_preview_point` takes: `null` the output (what apps get), `"compare"` the camera picture before the chain and the
 * output in one frame (the split-wipe Compare), or the picture entering a filter. (The engine also knows `"input"`, the camera
 * picture alone; nothing in the window asks for it since Compare shows both.)
 */
export type PreviewPoint = null | "compare" | { filter: string };

export function samePoint(a: PreviewPoint, b: PreviewPoint): boolean {
  if (a === null || b === null || typeof a === "string" || typeof b === "string") return a === b;
  return a.filter === b.filter;
}

/** The filter card a point names, or `null` (the output and Compare name none). */
export function pointFilter(point: PreviewPoint): string | null {
  return point !== null && typeof point === "object" ? point.filter : null;
}

/**
 * The caption on the preview while a point is set; `null` for the output. `steps` is the chain as `{ id, label }`: the
 * caption of a filter says which one, as "Crop/Pad · step 2 · showing the picture before this filter".
 */
export function pointLabel(point: PreviewPoint, steps: readonly { id: string; label: string }[]): string | null {
  if (point === null || point === "compare") return null; // Compare labels its two sides itself
  const at = steps.findIndex((s) => s.id === point.filter);
  if (at < 0) return null;
  return `${steps[at].label} · step ${at + 1} · showing the picture before this filter`;
}

/**
 * Hold to compare: the button is held by the pointer or by the Space key, independently, and is held while either is.
 * A blur (focus or the window lost) lets go of both: a key up that never arrives must not leave the picture stuck.
 */
export interface Hold {
  pointer: boolean;
  key: boolean;
}

export type HoldEvent = "pointerdown" | "pointerup" | "keydown" | "keyup" | "blur";

export const NOT_HELD: Hold = { pointer: false, key: false };

export function holdStep(hold: Hold, event: HoldEvent): Hold {
  switch (event) {
    case "pointerdown":
      return hold.pointer ? hold : { ...hold, pointer: true };
    case "pointerup":
      return hold.pointer ? { ...hold, pointer: false } : hold;
    case "keydown":
      return hold.key ? hold : { ...hold, key: true };
    case "keyup":
      return hold.key ? { ...hold, key: false } : hold;
    case "blur":
      return hold.pointer || hold.key ? NOT_HELD : hold;
  }
}

export const isHeld = (hold: Hold) => hold.pointer || hold.key;

export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * Where the canvas is drawn inside the preview box, which letterboxes it (`object-fit: contain`): the scale from canvas
 * pixels to CSS pixels and the offset of its top left corner in the box.
 */
export function letterbox(box: { width: number; height: number }, canvas: { width: number; height: number }) {
  const s = Math.min(box.width / canvas.width, box.height / canvas.height);
  return { s, ox: (box.width - canvas.width * s) / 2, oy: (box.height - canvas.height * s) / 2 };
}

/**
 * The point on the tapped picture under a pointer, in source pixels and not rounded: 0 to the source size, edges included, clamped to
 * the picture (the crop handles: a window edge may lie on the right or bottom edge, which no pixel index reaches).
 */
export function toSourceEdge(
  clientX: number,
  clientY: number,
  box: Rect,
  canvas: { width: number; height: number },
  source: { width: number; height: number },
): { x: number; y: number } {
  const { s, ox, oy } = letterbox(box, canvas);
  const cx = Math.min(Math.max((clientX - box.left - ox) / s, 0), canvas.width);
  const cy = Math.min(Math.max((clientY - box.top - oy) / s, 0), canvas.height);
  return { x: (cx * source.width) / canvas.width, y: (cy * source.height) / canvas.height };
}

/**
 * The pixel of the tapped picture under a pointer at (`clientX`, `clientY`), for the eyedropper and the crop handles, from the
 * box's rect `box`, the canvas size (the header's width and height) and the source size (the header's `source*`).
 * `null` when the pointer is in the letterbox, outside the picture; with `clamp` (a drag under way) it is the nearest edge pixel.
 */
export function toSourcePixel(
  clientX: number,
  clientY: number,
  box: Rect,
  canvas: { width: number; height: number },
  source: { width: number; height: number },
  clamp = false,
): { x: number; y: number } | null {
  const { s, ox, oy } = letterbox(box, canvas);
  const cx = (clientX - box.left - ox) / s;
  const cy = (clientY - box.top - oy) / s;
  const inside = cx >= 0 && cx < canvas.width && cy >= 0 && cy < canvas.height;
  if (!inside && !clamp) return null;
  const x = Math.round((Math.min(Math.max(cx, 0), canvas.width) * source.width) / canvas.width);
  const y = Math.round((Math.min(Math.max(cy, 0), canvas.height) * source.height) / canvas.height);
  return { x: Math.min(x, source.width - 1), y: Math.min(y, source.height - 1) };
}
