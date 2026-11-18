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
// What runs when it is not what is saved

/** The capture fields of `SourceCfg`: a change to one of them reopens the camera. */
export const CAPTURE_KEYS = ["video_device_id", "res_type", "resolution", "frame_interval", "video_format", "hw_decode", "buffering"] as const;

/** Whether `patch` changes how the camera is opened (the other fields apply to the next frame). */
export const touchesCapture = (patch: Partial<SourceCfg>): boolean => CAPTURE_KEYS.some((k) => k in patch);

/** What the engine runs for the saved format when it is not the saved format: the device default of the camera. */
export interface InUse {
  /** What was saved, for a sentence: `1920×1080 YUY2 60 fps`. */
  asked: string;
  /** The capture fields the engine holds (the backend compares them with what it is sent, so they must be exact). */
  source: Pick<SourceCfg, "res_type" | "video_format" | "resolution" | "frame_interval">;
}

/** The saved source as the engine runs it: the in-use capture fields over the saved ones. */
export const effectiveSource = (saved: SourceCfg, inUse: InUse | null): SourceCfg => (inUse ? { ...saved, ...inUse.source } : saved);

/** What the settings ask for, in words: `1920×1080 YUY2 60 fps`, `YUY2` or `device default` (as the backend words it). */
export const describeSaved = (s: SourceCfg): string => {
  const custom = s.res_type === "custom";
  const size = custom ? parseSize(s.resolution) : null;
  const parts = [
    size ? `${size[0]}×${size[1]}` : null,
    s.video_format === "any" ? null : formatLabel(s.video_format),
    custom && s.frame_interval > 0 ? `${fpsLabel(s.frame_interval)} fps` : null,
  ].filter((p): p is string => p !== null);
  return parts.length ? parts.join(" ") : "device default";
};

/** Whether the saved settings ask for the device default already. */
export const isDeviceDefault = (s: SourceCfg): boolean => s.res_type === "preferred" && s.video_format === "any";

/** `format-fallback`: the engine opened the device default at the size and rate the camera delivers. */
export const inUseFromFallback = (f: { asked: string; width: number; height: number; interval: number }): InUse => ({
  asked: f.asked,
  source: { res_type: "preferred", video_format: "any", resolution: sizeKey(f.width, f.height), frame_interval: f.interval },
});

/** The user asked for the device default (after a format error): the saved size and rate stay in the fields. */
export const inUseDeviceDefault = (saved: SourceCfg): InUse => ({
  asked: describeSaved(saved),
  source: { res_type: "preferred", video_format: "any", resolution: saved.resolution, frame_interval: saved.frame_interval },
});

/**
 * The saved settings after the user edited the camera fields while a fallback runs: the edit starts from what the
 * fields show (the in-use values), and is the new saved choice. Without a fallback it is the plain merge.
 */
export const savedAfterEdit = (saved: SourceCfg, inUse: InUse | null, patch: Partial<SourceCfg>): SourceCfg =>
  ({ ...(touchesCapture(patch) ? effectiveSource(saved, inUse) : saved), ...patch });

/** Pause follows `engine-paused`; a stop clears it, except the one announced for a virtual camera change. */
export const pausedAfterState = (paused: boolean, e: { state: string; reason?: string | null }): boolean =>
  e.state === "stopped" && e.reason !== "vcam-change" ? false : paused;

/** The in-use format goes with a stop, except the one announced for a virtual camera change (the engine comes back with it). */
export const inUseAfterState = (inUse: InUse | null, e: { state: string; reason?: string | null }): InUse | null =>
  e.state === "stopped" && e.reason !== "vcam-change" ? null : inUse;

/**
 * Who holds the camera, in words (`engine-state.apps`): `Discord.exe`, `Discord.exe and chrome.exe`,
 * `Discord.exe, chrome.exe and 2 more`; `another app` when the engine could name none.
 */
export const cameraUsers = (apps?: readonly string[] | null): string => {
  const a = apps ?? [];
  if (a.length === 0) return "another app";
  if (a.length === 1) return a[0];
  if (a.length === 2) return `${a[0]} and ${a[1]}`;
  return `${a[0]}, ${a[1]} and ${a.length - 2} more`;
};

/**
 * What the window says while the camera is held by another program. `maybe`: the list may include a program that only
 * reads Better Cam's virtual camera. Nothing is named, nothing is doubted.
 */
export const inUseText = (apps?: readonly string[] | null, maybe?: boolean): string =>
  `The camera ${maybe && apps?.length ? "may be" : "is"} in use by ${cameraUsers(apps)}.`;

/** Whether source, output and chain edits go to the backend: while the engine runs, and while the virtual camera is changed (it parks them). */
export const shouldPushSettings = (engineState: string, vcamBusy: string | null): boolean => engineState !== "stopped" || vcamBusy !== null;

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

// ---------------------------------------------------------------------------------------------------------------
// Device controls (plan 5.5, 6.2): what the camera reports, how it is shown, and what is stored per camera

/** The controls of plan 6.2, in the order the card lists them. */
export const CONTROL_KEYS = [
  "brightness", "contrast", "hue", "saturation", "sharpness", "gamma", "color_enable", "white_balance", "backlight_compensation",
  "gain", "powerline_frequency", "pan", "tilt", "roll", "zoom", "exposure", "iris", "focus", "low_light_compensation",
] as const;
export type ControlKey = (typeof CONTROL_KEYS)[number];

export const CONTROL_LABELS: Record<ControlKey, string> = {
  brightness: "Brightness",
  contrast: "Contrast",
  hue: "Hue",
  saturation: "Saturation",
  sharpness: "Sharpness",
  gamma: "Gamma",
  color_enable: "Color",
  white_balance: "White balance",
  backlight_compensation: "Backlight compensation",
  gain: "Gain",
  powerline_frequency: "Powerline frequency",
  pan: "Pan",
  tilt: "Tilt",
  roll: "Roll",
  zoom: "Zoom",
  exposure: "Exposure",
  iris: "Iris",
  focus: "Focus",
  low_light_compensation: "Low light compensation",
};

/** How a control is drawn: a slider, an on/off switch (0 or 1), or a list. */
export type ControlKind = "slider" | "switch" | "select";
export const controlKind = (key: ControlKey): ControlKind =>
  key === "color_enable" || key === "low_light_compensation" ? "switch" : key === "powerline_frequency" ? "select" : "slider";

/** One control as the device reports it (`get_camera_controls`, `set_camera_control`, `controls-changed`). */
export interface ControlInfo {
  key: ControlKey;
  min: number;
  max: number;
  step: number;
  /** The device's default. */
  def: number;
  value: number;
  auto: boolean;
  can_auto: boolean;
  can_manual: boolean;
}

/** Exposure is log2 of seconds: -6 is "1/64 s", -1 is "1/2 s", 0 is "1 s", 2 is "4 s". */
export const exposureText = (log2Seconds: number): string => {
  const v = Math.round(log2Seconds);
  return v < 0 ? `1/${2 ** -v} s` : `${2 ** v} s`;
};

export const whiteBalanceText = (kelvin: number): string => `${Math.round(kelvin)} K`;

/** Powerline frequency values (UVC): Auto (3) only exists on devices whose range reaches it. */
const POWERLINE_LABELS: Record<number, string> = { 0: "Disabled", 1: "50 Hz", 2: "60 Hz", 3: "Auto" };
export const powerlineOptions = (info: Pick<ControlInfo, "min" | "max">): Option[] =>
  [0, 1, 2, 3]
    .filter((v) => v >= info.min && v <= info.max)
    .map((v) => ({ value: String(v), label: POWERLINE_LABELS[v] }));

/** The text next to a control's label: a shutter time, kelvin, or the plain number. */
export const controlValueText = (info: Pick<ControlInfo, "key" | "value" | "min" | "max">): string => {
  switch (info.key) {
    case "exposure":
      return exposureText(info.value);
    case "white_balance":
      return whiteBalanceText(info.value);
    case "powerline_frequency":
      return powerlineOptions(info).find((o) => o.value === String(info.value))?.label ?? String(info.value);
    case "color_enable":
    case "low_light_compensation":
      return info.value ? "On" : "Off";
    default:
      return String(info.value);
  }
};

/** A control in the state the device starts in: Auto on where it has Auto, else its default value. Only other states are stored. */
export const isDefaultState = (i: Pick<ControlInfo, "value" | "def" | "auto" | "can_auto">): boolean =>
  i.auto === i.can_auto && (i.auto || i.value === i.def);

/** The device's values in the order of plan 6.2, and the ones it does not know dropped. */
export const sortControls = (infos: ControlInfo[]): ControlInfo[] =>
  CONTROL_KEYS.flatMap((k) => infos.filter((i) => i.key === k));

/** What the user changed on one control. `value` is kept while Auto is on (the last manual value). */
export interface StoredControl {
  value: number;
  auto: boolean;
}

/** The changed controls of one camera, with the name the camera had (a camera on another USB port has another id). */
export interface StoredDevice {
  name: string;
  values: Partial<Record<ControlKey, StoredControl>>;
}

/** Plan 5.1 `controls`, keyed by the camera's id (symbolic link). */
export type StoredControls = Record<string, StoredDevice>;

/** The stored camera that `cam` is: the same id, else the same vid/pid and name (it moved to another port). */
export const findStored = (stored: StoredControls, cam: { id: string; name: string }): { id: string; device: StoredDevice } | null => {
  const exact = stored[cam.id] ? cam.id : Object.keys(stored).find((id) => linkKey(id) === linkKey(cam.id));
  if (exact) return { id: exact, device: stored[exact] };
  const vp = vidPid(cam.id);
  if (!vp) return null;
  const id = Object.keys(stored).find((k) => vidPid(k) === vp && stored[k].name.toLowerCase() === cam.name.toLowerCase());
  return id ? { id, device: stored[id] } : null;
};

/** `stored` with `info` noted for `cam`: nothing is kept for a control in the device's own state. A moved camera is re-keyed. */
export const rememberControl = (
  stored: StoredControls,
  cam: { id: string; name: string },
  info: Pick<ControlInfo, "key" | "value" | "def" | "auto" | "can_auto">,
): StoredControls => {
  const found = findStored(stored, cam);
  const next = { ...stored };
  if (found && found.id !== cam.id) delete next[found.id];
  const values = { ...(found?.device.values ?? {}) };
  if (isDefaultState(info)) delete values[info.key];
  else values[info.key] = { value: info.value, auto: info.auto };
  if (Object.keys(values).length === 0) delete next[cam.id];
  else next[cam.id] = { name: cam.name, values };
  return next;
};

/** `stored` without the camera's changes (Reset all). */
export const forgetControls = (stored: StoredControls, cam: { id: string; name: string }): StoredControls => {
  const found = findStored(stored, cam);
  if (!found) return stored;
  const next = { ...stored };
  delete next[found.id];
  return next;
};

/** The `controls` argument of `start_engine` and `set_source`: the stored changes of `cam`, even if there are none. */
export const controlsRecord = (stored: StoredControls, cam: { id: string; name: string } | null) => {
  if (!cam) return null;
  return { id: cam.id, name: cam.name, controls: findStored(stored, cam)?.device.values ?? {} };
};

/** Reads the saved `controls`: every entry is checked, so a hand-edited or old save can't send the backend something it rejects. */
export function normalizeControls(raw: unknown): StoredControls {
  const out: StoredControls = {};
  if (!raw || typeof raw !== "object") return out;
  for (const [id, dev] of Object.entries(raw as Record<string, unknown>)) {
    if (!dev || typeof dev !== "object") continue;
    const d = dev as Record<string, unknown>;
    const values: StoredDevice["values"] = {};
    for (const key of CONTROL_KEYS) {
      const v = (d.values && typeof d.values === "object" ? (d.values as Record<string, unknown>)[key] : undefined) as
        | Record<string, unknown>
        | undefined;
      if (v && Number.isInteger(v.value) && typeof v.auto === "boolean") values[key] = { value: v.value as number, auto: v.auto };
    }
    if (Object.keys(values).length > 0) out[id] = { name: typeof d.name === "string" ? d.name : "", values };
  }
  return out;
}
