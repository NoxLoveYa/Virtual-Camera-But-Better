/**
 * Source and output settings (plan 5.2, 6.1, 6.3), the camera list the backend returns, and the pure helpers the
 * panels and the store use: frame interval <-> fps labels, which sizes/formats/rates a camera offers, colour
 * conversion and repairing a config against a device. Nothing here touches React, Tauri or the DOM, so a script can
 * import it.
 */

export const VIDEO_FORMATS = [
  "mjpeg", "h264", "hevc", "nv12", "yuy2", "i420", "yv12", "y800", "p010", "yvyu", "uyvy", "hdyc", "xrgb", "argb", "rgb24",
] as const;
export type VideoFormat = "any" | (typeof VIDEO_FORMATS)[number];

export type ResType = "preferred" | "custom";
export type ColorSpace = "default" | "709" | "601" | "2100PQ" | "2100HLG";
export type ColorRange = "default" | "partial" | "full";
export type Buffering = "auto" | "on" | "off";

/** Keys are the OBS `dshow_input` setting names; the backend reads them as they are. */
export interface SourceCfg {
  video_device_id: string | null;
  res_type: ResType;
  resolution: string;
  /** 100 ns units; -1 = match the output FPS, 0 = highest. */
  frame_interval: number;
  video_format: VideoFormat;
  color_space: ColorSpace;
  color_range: ColorRange;
  buffering: Buffering;
  flip_vertically: boolean;
  autorotation: boolean;
  hw_decode: boolean;
  deactivate_when_not_showing: boolean;
}

export type FitMode = "fit" | "fill" | "stretch";

export interface OutputCfg {
  /** "source" or "WxH". */
  resolution: string;
  /** "source" or a frame rate. */
  fps: "source" | number;
  fit: FitMode;
  /** OBS int 0xAABBGGRR. */
  background_color: number;
  extra_sizes: boolean;
}

export interface CameraFormat {
  format: string;
  width: number;
  height: number;
  /** 100 ns units, ascending (the highest rate first). */
  intervals: number[];
}

export interface CameraInfo {
  id: string;
  name: string;
  formats: CameraFormat[];
}

export const DEFAULT_SOURCE: SourceCfg = {
  video_device_id: null,
  res_type: "preferred",
  resolution: "1920x1080",
  frame_interval: -1,
  video_format: "any",
  color_space: "default",
  color_range: "default",
  buffering: "auto",
  flip_vertically: false,
  autorotation: true,
  hw_decode: true,
  deactivate_when_not_showing: true,
};

export const OPAQUE_BLACK = 0xff000000;

export const DEFAULT_OUTPUT: OutputCfg = {
  resolution: "source",
  fps: "source",
  fit: "fit",
  background_color: OPAQUE_BLACK,
  extra_sizes: true,
};

// ---------------------------------------------------------------------------------------------------------------
// Option tables (value is always a string, as the Select widget wants)

export interface Option {
  value: string;
  label: string;
}

export const RES_TYPE_OPTIONS: Option[] = [
  { value: "preferred", label: "Device default" },
  { value: "custom", label: "Custom" },
];

export const COLOR_SPACE_OPTIONS: Option[] = [
  { value: "default", label: "Default" },
  { value: "709", label: "Rec. 709" },
  { value: "601", label: "Rec. 601" },
  { value: "2100PQ", label: "Rec. 2100 (PQ)" },
  { value: "2100HLG", label: "Rec. 2100 (HLG)" },
];

export const COLOR_RANGE_OPTIONS: Option[] = [
  { value: "default", label: "Default" },
  { value: "partial", label: "Limited" },
  { value: "full", label: "Full" },
];

export const BUFFERING_OPTIONS: Option[] = [
  { value: "auto", label: "Auto-detect" },
  { value: "on", label: "Enable" },
  { value: "off", label: "Disable" },
];

export const OUTPUT_RESOLUTION_OPTIONS: Option[] = [
  { value: "source", label: "Same as camera" },
  ...["3840x2160", "2560x1440", "1920x1080", "1280x720", "960x540", "640x360", "640x480"].map((v) => ({ value: v, label: v })),
];

export const OUTPUT_FPS_VALUES = [15, 24, 25, 30, 50, 60] as const;
export const OUTPUT_FPS_OPTIONS: Option[] = [
  { value: "source", label: "Same as camera" },
  ...OUTPUT_FPS_VALUES.map((v) => ({ value: String(v), label: String(v) })),
];

export const FIT_OPTIONS: Option[] = [
  { value: "fit", label: "Fit (bars)" },
  { value: "fill", label: "Fill (crop)" },
  { value: "stretch", label: "Stretch" },
];

/** The two frame-rate entries that are not a device interval. */
export const MATCH_OUTPUT_FPS = -1;
export const HIGHEST_FPS = 0;

// ---------------------------------------------------------------------------------------------------------------
// Sizes, intervals, formats

/** "1920x1080" (also "1920×1080") to [w, h]; null when it is not a size. */
export const parseSize = (s: string): [number, number] | null => {
  const m = /^\s*(\d{1,5})\s*[x×]\s*(\d{1,5})\s*$/i.exec(s);
  if (!m) return null;
  const w = Number(m[1]);
  const h = Number(m[2]);
  return w > 0 && h > 0 ? [w, h] : null;
};

export const sizeKey = (w: number, h: number) => `${w}x${h}`;

/** Frame rate of a frame interval in 100 ns units. */
export const intervalFps = (interval: number) => 1e7 / interval;

/** "60", "30", "29.97": whole numbers stay whole, anything else gets two decimals (trailing zeros dropped). */
export const fpsLabel = (interval: number): string => {
  const fps = intervalFps(interval);
  const whole = Math.round(fps);
  return Math.abs(fps - whole) < 0.01 ? String(whole) : String(Number(fps.toFixed(2)));
};

/** A measured rate for the footer: one decimal, ".0" dropped. */
export const measuredFps = (fps: number): string => String(Number(fps.toFixed(1)));

export const formatLabel = (format: string) => format.toUpperCase();

const area = (s: [number, number]) => s[0] * s[1];

/** Distinct sizes of the camera for a format ("any" = every format), largest first. */
export const cameraSizes = (cam: CameraInfo, format: VideoFormat): string[] => {
  const seen = new Map<string, [number, number]>();
  for (const f of cam.formats) {
    if (format === "any" || f.format === format) seen.set(sizeKey(f.width, f.height), [f.width, f.height]);
  }
  return [...seen.entries()].sort(([, a], [, b]) => area(b) - area(a) || b[0] - a[0]).map(([k]) => k);
};

/** Distinct formats of the camera in the order it lists them, for one size or (without `size`) for any size. */
export const cameraFormats = (cam: CameraInfo, size?: string): string[] => {
  const out: string[] = [];
  for (const f of cam.formats) {
    if (size !== undefined && sizeKey(f.width, f.height) !== size) continue;
    if (!out.includes(f.format)) out.push(f.format);
  }
  return out;
};

/** Frame intervals the camera offers at a size and format ("any" = the union over formats), ascending. */
export const cameraIntervals = (cam: CameraInfo, size: string, format: VideoFormat): number[] => {
  const all = new Set<number>();
  for (const f of cam.formats) {
    if (sizeKey(f.width, f.height) === size && (format === "any" || f.format === format)) f.intervals.forEach((i) => all.add(i));
  }
  return [...all].sort((a, b) => a - b);
};

const isWide = ([w, h]: [number, number]) => Math.abs(w / h - 16 / 9) < 0.02;

/** The device's largest 16:9 size, else its largest size; null for a camera whose formats could not be read. */
export const largestWide = (cam: CameraInfo): string | null => {
  const sizes = cameraSizes(cam, "any");
  return sizes.find((k) => isWide(parseSize(k)!)) ?? sizes[0] ?? null;
};

/** The listed size closest to `target`: the same aspect first, then the nearest pixel count. */
export const nearestSize = (sizes: string[], target: string): string | null => {
  const t = parseSize(target);
  if (sizes.length === 0) return null;
  if (!t) return sizes.find((k) => isWide(parseSize(k)!)) ?? sizes[0];
  let best = sizes[0];
  let bestScore = Infinity;
  for (const k of sizes) {
    const s = parseSize(k)!;
    const score = 4 * Math.abs(Math.log(s[0] / s[1] / (t[0] / t[1]))) + Math.abs(Math.log(area(s) / area(t)));
    if (score < bestScore) {
      best = k;
      bestScore = score;
    }
  }
  return best;
};

/** The interval whose frame rate is closest to the one of `target`. */
export const nearestInterval = (intervals: number[], target: number): number | null => {
  if (intervals.length === 0 || target <= 0) return null;
  const want = intervalFps(target);
  return intervals.reduce((a, b) => (Math.abs(intervalFps(b) - want) < Math.abs(intervalFps(a) - want) ? b : a));
};

// ---------------------------------------------------------------------------------------------------------------
// Finding the camera a config names

/** The link without its reference string (`\global`), lower case: the part Windows keeps stable. */
const linkKey = (id: string) => id.toLowerCase().replace(/\\[^\\]*$/, "");
const vidPid = (id: string) => /vid_[0-9a-f]{4}&pid_[0-9a-f]{4}/i.exec(id)?.[0].toLowerCase() ?? null;

/** The camera `id` names: the same link, else the only camera with the same vid/pid (the device moved to another port). */
export const findCamera = (cams: CameraInfo[], id: string | null): CameraInfo | null => {
  if (!id) return null;
  const key = linkKey(id);
  const exact = cams.find((c) => c.id === id || linkKey(c.id) === key);
  if (exact) return exact;
  const vp = vidPid(id);
  const same = vp ? cams.filter((c) => vidPid(c.id) === vp) : [];
  return same.length === 1 ? same[0] : null;
};

// ---------------------------------------------------------------------------------------------------------------
// Repair

/**
 * Makes `src` consistent with the camera it names, changing as little as possible:
 * - no camera chosen: the first one (and, with that first choice, its largest 16:9 size);
 * - the saved camera is not connected: nothing changes (the user's settings wait for it);
 * - a format the camera lacks becomes "any"; with Custom a size or rate the camera lacks (for that format) becomes
 *   the nearest one it has;
 * - a camera whose formats could not be read can only use the device default.
 */
export function repairSource(src: SourceCfg, cams: CameraInfo[]): SourceCfg {
  const s = { ...src };
  const firstChoice = !s.video_device_id;
  const cam = firstChoice ? cams[0] : findCamera(cams, s.video_device_id);
  if (!cam) return s;
  s.video_device_id = cam.id;

  if (cam.formats.length === 0) {
    s.res_type = "preferred";
    s.video_format = "any";
    return s;
  }
  if (firstChoice) s.resolution = largestWide(cam) ?? s.resolution;
  if (s.video_format !== "any" && !cameraFormats(cam).includes(s.video_format)) s.video_format = "any";

  // The stored size also has to be one the camera has under Device default, so a later switch to Custom starts valid.
  const custom = s.res_type === "custom";
  const sizes = cameraSizes(cam, custom ? s.video_format : "any");
  if (!sizes.includes(s.resolution)) s.resolution = nearestSize(sizes, s.resolution) ?? s.resolution;
  if (custom && s.frame_interval > 0) {
    const rates = cameraIntervals(cam, s.resolution, s.video_format);
    if (!rates.includes(s.frame_interval)) s.frame_interval = nearestInterval(rates, s.frame_interval) ?? MATCH_OUTPUT_FPS;
  }
  return s;
}

// ---------------------------------------------------------------------------------------------------------------
// Colour

const clampByte = (n: number) => Math.max(0, Math.min(255, Math.round(n)));

/** OBS 0xAABBGGRR to "#rrggbb" (alpha is dropped: the background is opaque). */
export const abgrToHex = (abgr: number): string => {
  const n = abgr >>> 0;
  const part = (shift: number) => ((n >>> shift) & 0xff).toString(16).padStart(2, "0");
  return `#${part(0)}${part(8)}${part(16)}`;
};

/** "#rrggbb" to OBS 0xAABBGGRR with alpha 0xFF, as an unsigned number. Anything else gives opaque black. */
export const hexToAbgr = (hex: string): number => {
  const m = /^#?([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex.trim());
  if (!m) return OPAQUE_BLACK;
  const [r, g, b] = [m[1], m[2], m[3]].map((h) => clampByte(parseInt(h, 16)));
  return (0xff000000 | (b << 16) | (g << 8) | r) >>> 0;
};

// ---------------------------------------------------------------------------------------------------------------
// Reading saved data: every field is checked, so a hand-edited or old save can't send the backend something it rejects

const oneOf = <T extends string>(v: unknown, allowed: readonly T[], def: T): T => (allowed.includes(v as T) ? (v as T) : def);
const bool = (v: unknown, def: boolean) => (typeof v === "boolean" ? v : def);

const SIZE = /^\d{1,5}x\d{1,5}$/;

export function normalizeSource(raw: unknown): SourceCfg {
  const r = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const d = DEFAULT_SOURCE;
  return {
    video_device_id: typeof r.video_device_id === "string" && r.video_device_id ? r.video_device_id : null,
    res_type: oneOf(r.res_type, ["preferred", "custom"], d.res_type),
    resolution: typeof r.resolution === "string" && SIZE.test(r.resolution) ? r.resolution : d.resolution,
    frame_interval: Number.isInteger(r.frame_interval) && (r.frame_interval as number) >= -1 ? (r.frame_interval as number) : d.frame_interval,
    video_format: oneOf(r.video_format, ["any", ...VIDEO_FORMATS], d.video_format),
    color_space: oneOf(r.color_space, ["default", "709", "601", "2100PQ", "2100HLG"], d.color_space),
    color_range: oneOf(r.color_range, ["default", "partial", "full"], d.color_range),
    buffering: oneOf(r.buffering, ["auto", "on", "off"], d.buffering),
    flip_vertically: bool(r.flip_vertically, d.flip_vertically),
    autorotation: bool(r.autorotation, d.autorotation),
    hw_decode: bool(r.hw_decode, d.hw_decode),
    deactivate_when_not_showing: bool(r.deactivate_when_not_showing, d.deactivate_when_not_showing),
  };
}

export function normalizeOutput(raw: unknown): OutputCfg {
  const r = (raw && typeof raw === "object" ? raw : {}) as Record<string, unknown>;
  const d = DEFAULT_OUTPUT;
  const fps = r.fps;
  return {
    resolution: r.resolution === "source" || (typeof r.resolution === "string" && SIZE.test(r.resolution)) ? (r.resolution as string) : d.resolution,
    fps: typeof fps === "number" && Number.isInteger(fps) && fps >= 1 && fps <= 240 ? fps : "source",
    fit: oneOf(r.fit, ["fit", "fill", "stretch"], d.fit),
    background_color:
      typeof r.background_color === "number" && Number.isInteger(r.background_color) && r.background_color >= -(2 ** 31) && r.background_color <= 2 ** 32 - 1
        ? r.background_color >>> 0
        : d.background_color,
    extra_sizes: bool(r.extra_sizes, d.extra_sizes),
  };
}
