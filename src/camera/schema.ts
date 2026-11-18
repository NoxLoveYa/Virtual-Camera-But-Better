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

/** The running format a Start announced, kept only when the backend accepted the Start (a rejected one runs nothing). */
export const inUseAfterStart = (requested: InUse | null, accepted: boolean): InUse | null => (accepted ? requested : null);

/** The state that goes with a running engine: what a removal of the virtual camera clears when it happened. */
export interface RunningState {
  paused: boolean;
  inUse: InUse | null;
  wasRunning: boolean;
}

/**
 * The running state after the virtual camera was removed (`removed`: the uninstall call succeeded) or not. The backend
 * shuts the engine down before it asks for administrator rights and brings it back with the same state when the
 * removal is declined or fails, so only a removal that happened ends the pause, the fallback and the flag that starts
 * the camera at the next launch; otherwise they stay as they were.
 */
export const afterVcamRemoval = (removed: boolean, s: RunningState): RunningState =>
  removed ? { paused: false, inUse: null, wasRunning: false } : { paused: s.paused, inUse: s.inUse, wasRunning: s.wasRunning };

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

/** `Camera disconnected (0xC00D3EA2)` to `Camera disconnected`: hex codes belong in a tooltip, never in visible text. */
export const stripCodes = (s: string): string => s.replace(/\s*\(0x[0-9A-Fa-f]{8}\)/g, "");

const sentence = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/**
 * Which switch the engine's words for a privacy wait name. The engine says "privacy" only for an open that Windows
 * refused (access denied) while a switch is off; the switches only decide this wording.
 */
const privacyText = (reason: string | null): string => {
  const r = reason ?? "";
  const afterwards = "Better Cam goes live by itself afterwards.";
  if (r.includes("policy")) {
    return `Windows is not letting apps use the camera: a Windows policy turns camera access off, and no setting in Windows Settings changes it. ${afterwards}`;
  }
  if (r.includes("desktop apps")) {
    return `Windows is not letting desktop apps use the camera. Turn on “Let desktop apps access your camera” in Windows camera privacy settings. ${afterwards}`;
  }
  return `Windows is not letting apps use the camera. Turn on “Camera access” in Windows camera privacy settings. ${afterwards}`;
};

/**
 * What the window says while the camera is waited for, by the engine's `kind` (plan 4.9): `long` is the banner's text,
 * `short` the preview's. Neither shows a hex code; the engine's own `reason` belongs in a tooltip.
 */
export const problemText = (
  kind: string | null,
  reason: string | null,
  apps?: readonly string[] | null,
  appsMaybe?: boolean,
): { short: string; long: string } => {
  const plain = reason ? sentence(stripCodes(reason)) : null;
  switch (kind) {
    case "in_use":
      return { short: inUseText(apps, appsMaybe), long: `${inUseText(apps, appsMaybe)} Better Cam goes live as soon as it is free.` };
    case "disconnected":
      return {
        short: "Camera disconnected",
        long: "The camera was disconnected. Plug it in again; Better Cam goes live as soon as it is back.",
      };
    case "not_found":
      return {
        short: "Camera not found",
        long: "Camera not found. Connect it or pick another camera; Better Cam goes live as soon as it is there.",
      };
    case "privacy":
      return { short: "Camera blocked by Windows privacy settings", long: privacyText(reason) };
    default:
      return {
        short: plain ?? "The camera stopped sending pictures",
        long: `${plain?.replace(/[.!]+$/, "") ?? "The camera is not answering"}. Better Cam keeps trying for a little while.`,
      };
  }
};

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

/** Who sets a control: the user (`manual`), the camera's own Auto flag (`camera`), or Better Cam's controller (`app`). */
export type ControlMode = "manual" | "camera" | "app";
export const CONTROL_MODES: readonly ControlMode[] = ["manual", "camera", "app"];
export const MODE_LABELS: Record<ControlMode, string> = { manual: "Manual", camera: "Camera auto", app: "Better Cam auto" };

/** The controls Better Cam auto drives (the gain follows the exposure loop, 2.2 of docs/AUTO-CONTROLS.md). */
const APP_KEYS: readonly ControlKey[] = ["exposure", "white_balance", "gain"];

/**
 * One control as the device reports it (`get_camera_controls`, `set_camera_control`, `controls-changed`; the backend's
 * `ControlInfoDto`). The last four fields are Better Cam auto's (docs/AUTO-CONTROLS.md 5.3); an older backend sends none.
 */
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
  mode?: ControlMode;
  /** Better Cam auto is offered (`stops`: whole stops only); `null` when it is not, and for controls that have none. */
  app?: "full" | "stops" | null;
  /** Why Better Cam auto is not offered. */
  app_why?: string;
  /** `exposure` for the gain while the exposure loop sets it. */
  driven_by?: "exposure";
}

/** The mode of a control: what the backend says, else what the Auto flag says. */
export const modeOf = (i: Pick<ControlInfo, "auto"> & { mode?: ControlMode }): ControlMode => i.mode ?? (i.auto ? "camera" : "manual");

/** Whether the control has a mode list (Manual / Camera auto / Better Cam auto) instead of the Auto switch. The gain has none: it follows the exposure. */
export const hasModeList = (i: Pick<ControlInfo, "key" | "app" | "app_why" | "mode">): boolean =>
  i.key !== "gain" && (i.app != null || i.app_why != null || i.mode === "app");

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

/**
 * A control in the state the device starts in: Auto on where it has Auto, else its default value. Only other states
 * are stored. Better Cam auto is never the device's state.
 */
export const isDefaultState = (i: Pick<ControlInfo, "value" | "def" | "auto" | "can_auto"> & { mode?: ControlMode }): boolean =>
  i.mode !== "app" && i.auto === i.can_auto && (i.auto || i.value === i.def);

/** The device's values in the order of plan 6.2, and the ones it does not know dropped. */
export const sortControls = (infos: ControlInfo[]): ControlInfo[] =>
  CONTROL_KEYS.flatMap((k) => infos.filter((i) => i.key === k));

/**
 * What the user changed on one control. `value` is kept while Auto is on (the last manual value); for `mode: "app"`
 * (Better Cam auto) it is the resume point, which the controller keeps up to date and the engine writes on open.
 */
export interface StoredControl {
  value: number;
  /** The camera's Auto flag; false for `app`. */
  auto: boolean;
  /** Absent: Manual or Camera auto, as `auto` says. A build without Better Cam auto reads an `app` control as Manual at `value`. */
  mode?: "app";
}

export type Speed = "slow" | "normal" | "fast";
export const SPEEDS: readonly Speed[] = ["slow", "normal", "fast"];
export const SPEED_OPTIONS: Option[] = [
  { value: "slow", label: "Slow" },
  { value: "normal", label: "Normal" },
  { value: "fast", label: "Fast" },
];

/** Better Cam auto's settings of one camera (docs/AUTO-CONTROLS.md 5.1). They are not in presets. */
export interface AutoCfg {
  exposure: {
    /** 0.2..0.8: the sRGB-coded mean brightness to hold (Auto Adjust's scale). */
    target: number;
    /** `"fit"` (the frame time) or a whole stop in log2 seconds. */
    longest: "fit" | number;
    /** In device gain units; `null`: 75 % of the range. */
    max_gain: number | null;
    speed: Speed;
  };
  white_balance: {
    /** -1..1, +1 is warmer. */
    warmth: number;
    speed: Speed;
  };
}

export const DEFAULT_AUTO: AutoCfg = {
  exposure: { target: 0.45, longest: "fit", max_gain: null, speed: "normal" },
  white_balance: { warmth: 0, speed: "normal" },
};

export const TARGET_MIN = 0.2;
export const TARGET_MAX = 0.8;

/** What the controller learned about one camera (kept for next time; the backend sends it with `auto-status`). */
export interface Learned {
  /** Stops across the whole gain range, 0.5..6. */
  gain_stops?: number;
  /** Colour signal per mired, 0.002..0.02. */
  wb_alpha?: number;
  /** Capture fps (rounded, as text) to the longest exposure that held it. */
  rate_caps?: Record<string, number>;
  /** Controls whose Better Cam auto is off because the camera ignored it; "Try again" clears this. */
  blocked?: ("exposure" | "white_balance")[];
}

/** The changed controls of one camera, with the name the camera had (a camera on another USB port has another id). */
export interface StoredDevice {
  name: string;
  values: Partial<Record<ControlKey, StoredControl>>;
  /** Kept when no control is in Better Cam auto, so switching back restores the targets. */
  auto?: AutoCfg;
  learned?: Learned;
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

/** A camera's record with nothing in it is not kept. */
const isEmptyDevice = (d: StoredDevice) => Object.keys(d.values).length === 0 && !d.auto && !d.learned;

/** `stored` with the record of `cam` edited (an empty one is dropped). A moved camera is re-keyed. */
const editDevice = (stored: StoredControls, cam: { id: string; name: string }, edit: (d: StoredDevice) => StoredDevice): StoredControls => {
  const found = findStored(stored, cam);
  const next = { ...stored };
  if (found && found.id !== cam.id) delete next[found.id];
  const d = edit(found?.device ?? { name: cam.name, values: {} });
  if (isEmptyDevice(d)) delete next[cam.id];
  else next[cam.id] = { ...d, name: cam.name };
  return next;
};

/**
 * `stored` with `info` noted for `cam`: nothing is kept for a control in the device's own state. A moved camera is
 * re-keyed. Better Cam auto is kept as `mode: "app"` with the value as the resume point; the camera's Better Cam auto
 * settings and learned values stay as they are.
 */
export const rememberControl = (
  stored: StoredControls,
  cam: { id: string; name: string },
  info: Pick<ControlInfo, "key" | "value" | "def" | "auto" | "can_auto"> & { mode?: ControlMode },
): StoredControls =>
  editDevice(stored, cam, (d) => {
    const values = { ...d.values };
    if (isDefaultState(info)) delete values[info.key];
    else values[info.key] = info.mode === "app" ? { value: info.value, auto: false, mode: "app" } : { value: info.value, auto: info.auto };
    return { ...d, values };
  });

/** `stored` without the camera's changed controls (Reset all). Its Better Cam auto settings and learned values stay for next time. */
export const forgetControls = (stored: StoredControls, cam: { id: string; name: string }): StoredControls =>
  findStored(stored, cam) ? editDevice(stored, cam, (d) => ({ ...d, values: {} })) : stored;

/** The Better Cam auto settings of `cam`: the stored ones, else the defaults. */
export const autoCfgOf = (stored: StoredControls, cam: { id: string; name: string } | null): AutoCfg =>
  (cam && findStored(stored, cam)?.device.auto) || DEFAULT_AUTO;

/** `stored` with the Better Cam auto settings of `cam` (the defaults are not kept). */
export const withAutoCfg = (stored: StoredControls, cam: { id: string; name: string }, cfg: AutoCfg): StoredControls =>
  editDevice(stored, cam, (d) => ({ ...d, auto: JSON.stringify(cfg) === JSON.stringify(DEFAULT_AUTO) ? undefined : cfg }));

/** `stored` with what the controller learned about `cam` (an empty `learned` removes it). */
export const withLearned = (stored: StoredControls, cam: { id: string; name: string }, learned: unknown): StoredControls => {
  const l = normalizeLearned(learned);
  return editDevice(stored, cam, (d) => ({ ...d, learned: l }));
};

/** What the controller reports on entering `holding` (`auto-status.resume`): where each loop starts next time. */
export interface Resume {
  exposure?: number;
  gain?: number;
  white_balance?: number;
}

/**
 * `stored` with the resume point of `cam`'s Better Cam auto controls updated. Only a control stored as `app` takes one
 * (a stale event after the user left Better Cam auto must not store a value); `stored` itself when nothing changes.
 */
export const withResume = (stored: StoredControls, cam: { id: string; name: string }, resume: Resume): StoredControls => {
  const device = findStored(stored, cam)?.device;
  if (!device) return stored;
  const values = { ...device.values };
  let changed = false;
  for (const key of ["exposure", "gain", "white_balance"] as const) {
    const v = resume[key];
    const cur = values[key];
    if (cur?.mode === "app" && typeof v === "number" && Number.isInteger(v) && cur.value !== v) {
      values[key] = { ...cur, value: v };
      changed = true;
    }
  }
  return changed ? editDevice(stored, cam, (d) => ({ ...d, values })) : stored;
};

/** The `controls` argument of `start_engine` and `set_source`: the stored record of `cam` (controls, Better Cam auto settings, learned values), even if there is none. */
export const controlsRecord = (stored: StoredControls, cam: { id: string; name: string } | null) => {
  if (!cam) return null;
  const d = findStored(stored, cam)?.device;
  return { id: cam.id, name: cam.name, controls: d?.values ?? {}, ...(d?.auto && { auto: d.auto }), ...(d?.learned && { learned: d.learned }) };
};

// ---------------------------------------------------------------------------------------------------------------
// Reading what is saved

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));
const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** The exposure range Better Cam auto accepts for "Longest exposure": whole stops of log2 seconds (the backend's own clamp). */
const LONGEST_MIN = -16;
const LONGEST_MAX = 5;

/** Reads `AutoCfg` with every field checked; a bad field is its default, a non-object is no settings (`undefined`). */
export function normalizeAutoCfg(raw: unknown): AutoCfg | undefined {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return undefined;
  const r = raw as Record<string, unknown>;
  const obj = (v: unknown) => (v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {});
  const e = obj(r.exposure);
  const w = obj(r.white_balance);
  const d = DEFAULT_AUTO;
  return {
    exposure: {
      target: finite(e.target) ? clamp(e.target, TARGET_MIN, TARGET_MAX) : d.exposure.target,
      longest:
        Number.isInteger(e.longest) && (e.longest as number) >= LONGEST_MIN && (e.longest as number) <= LONGEST_MAX ? (e.longest as number) : "fit",
      max_gain: Number.isInteger(e.max_gain) && (e.max_gain as number) >= 0 && (e.max_gain as number) <= 100000 ? (e.max_gain as number) : null,
      speed: oneOf(e.speed, SPEEDS, d.exposure.speed),
    },
    white_balance: {
      warmth: finite(w.warmth) ? clamp(w.warmth, -1, 1) : d.white_balance.warmth,
      speed: oneOf(w.speed, SPEEDS, d.white_balance.speed),
    },
  };
}

/** Reads `Learned` with every value inside its range and unknown keys dropped; `undefined` when nothing valid is left. */
export function normalizeLearned(raw: unknown): Learned | undefined {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return undefined;
  const r = raw as Record<string, unknown>;
  const out: Learned = {};
  if (finite(r.gain_stops)) out.gain_stops = clamp(r.gain_stops, 0.5, 6);
  if (finite(r.wb_alpha)) out.wb_alpha = clamp(r.wb_alpha, 0.002, 0.02);
  if (r.rate_caps && typeof r.rate_caps === "object" && !Array.isArray(r.rate_caps)) {
    const caps: Record<string, number> = {};
    for (const [fps, stop] of Object.entries(r.rate_caps as Record<string, unknown>)) {
      if (/^\d{1,3}$/.test(fps) && finite(stop)) caps[fps] = clamp(Math.round(stop), LONGEST_MIN, LONGEST_MAX);
    }
    if (Object.keys(caps).length > 0) out.rate_caps = caps;
  }
  if (Array.isArray(r.blocked)) {
    const blocked = (["exposure", "white_balance"] as const).filter((k) => (r.blocked as unknown[]).includes(k));
    if (blocked.length > 0) out.blocked = [...blocked];
  }
  return Object.keys(out).length > 0 ? out : undefined;
}

/**
 * Reads the saved `controls`: every entry is checked, so a hand-edited or old save can't send the backend something it
 * rejects. `mode: "app"` stays only on the controls Better Cam auto drives, with the Auto flag off, and on the gain only
 * while the exposure is in Better Cam auto too (the gain follows it); anything else about `mode` is dropped and the
 * control is Manual / Camera auto as before.
 */
export function normalizeControls(raw: unknown): StoredControls {
  const out: StoredControls = {};
  if (!raw || typeof raw !== "object") return out;
  for (const [id, dev] of Object.entries(raw as Record<string, unknown>)) {
    if (!dev || typeof dev !== "object") continue;
    const d = dev as Record<string, unknown>;
    const values: StoredDevice["values"] = {};
    const at = (key: ControlKey) =>
      (d.values && typeof d.values === "object" ? (d.values as Record<string, unknown>)[key] : undefined) as Record<string, unknown> | undefined;
    for (const key of CONTROL_KEYS) {
      const v = at(key);
      if (v && Number.isInteger(v.value) && typeof v.auto === "boolean") {
        values[key] = v.mode === "app" && !v.auto && APP_KEYS.includes(key) ? { value: v.value as number, auto: false, mode: "app" } : { value: v.value as number, auto: v.auto };
      }
    }
    // The gain is Better Cam auto only with the exposure.
    if (values.gain?.mode === "app" && values.exposure?.mode !== "app") values.gain = { value: values.gain.value, auto: false };
    const auto = normalizeAutoCfg(d.auto);
    const learned = normalizeLearned(d.learned);
    const device: StoredDevice = { name: typeof d.name === "string" ? d.name : "", values, ...(auto && { auto }), ...(learned && { learned }) };
    if (!isEmptyDevice(device)) out[id] = device;
  }
  return out;
}

// ---------------------------------------------------------------------------------------------------------------
// Better Cam auto on the card (docs/AUTO-CONTROLS.md 4.1-4.3)

/** The exposure loop's state (`auto-status.exposure.state`); the white balance loop has `at_limit` instead of the three limits. */
export type AutoState =
  | "starting" | "adjusting" | "holding" | "limit_gain" | "limit_exposure" | "too_bright" | "no_signal" | "at_limit"
  | "no_picture" | "paused" | "closed" | "conflict" | "ignored" | "no_response" | "not_answering" | "hunting";

/** Where a hand-back put the control. */
export type HandedBack = "camera" | "manual";

export interface ExposureStatus {
  state: AutoState;
  exposure: number;
  gain: number | null;
  /** The longest exposure now allowed (log2 seconds), and why it is there. */
  cap: number;
  cap_reason: "fps" | "user" | "learned";
  max_gain: number | null;
  /** The capture rate measured, and the rate asked for. */
  fps: number;
  target_fps: number;
  /** The text of 4.2, made by the backend. `null` when the state has none (the watchdog states draw theirs from `handed_back`). */
  message: string | null;
  handed_back?: HandedBack;
  /** The stripes note of 2.5.10, while it applies. */
  note?: string | null;
}

export interface WhiteBalanceStatus {
  state: AutoState;
  kelvin: number;
  message: string | null;
  handed_back?: HandedBack;
}

/** `auto-status` (docs/AUTO-CONTROLS.md 5.4). `learned` and `resume` are the store's to persist; they are not kept in the state. */
export interface AutoStatus {
  /** The camera. */
  id: string;
  exposure?: ExposureStatus;
  white_balance?: WhiteBalanceStatus;
  learned?: Learned;
  resume?: Resume;
}

/** The loop the card shows a status for, by the control row it sits under. */
export type AutoLoop = "exposure" | "white_balance";

/** The dot of the status line: neutral while it works, warning at a limit, error when it stopped. */
export type AutoTone = "neutral" | "warn" | "error";
export const autoTone = (state: AutoState): AutoTone => {
  switch (state) {
    case "limit_gain":
    case "limit_exposure":
    case "too_bright":
    case "no_signal":
    case "at_limit":
      return "warn";
    case "conflict":
    case "ignored":
    case "no_response":
    case "not_answering":
    case "hunting":
      return "error";
    default:
      return "neutral";
  }
};

/** The faults that end the loop and give the control back (`handed_back`), as opposed to `conflict`, which only pauses it. */
export const isHandBackState = (state: AutoState): boolean =>
  state === "ignored" || state === "no_response" || state === "not_answering" || state === "hunting";

/** The button of the status line: Resume after a conflict, Try again after the camera ignored Better Cam auto. */
export const autoAction = (state: AutoState): "resume" | "retry" | null => (state === "conflict" ? "resume" : state === "ignored" ? "retry" : null);

/** The status of `cam`'s loop, if it is about this camera (the backend names it by the link of its record; a camera that moved to another port is the same by vid/pid). */
export const statusOf = <L extends AutoLoop>(status: AutoStatus | null, cam: { id: string } | null, loop: L): NonNullable<AutoStatus[L]> | null => {
  if (!status || !cam) return null;
  const vp = vidPid(cam.id);
  const same = status.id === cam.id || linkKey(status.id) === linkKey(cam.id) || (vp !== null && vidPid(status.id) === vp);
  return same ? ((status[loop] ?? null) as NonNullable<AutoStatus[L]> | null) : null;
};

/**
 * Whether the status line of a loop shows for a control in `mode`: while it is in Better Cam auto, and after a
 * hand-back while the control is where it was handed to (that line carries "Try again"). Anything else is stale.
 */
export const showsStatus = (mode: ControlMode, s: { state: AutoState; handed_back?: HandedBack } | null): boolean =>
  !!s && (mode === "app" || (isHandBackState(s.state) && s.handed_back === mode));

/** The line under the row: the backend's text, else nothing (`no_picture` has none before 10 s). */
export const autoText = (s: { message: string | null }): string | null => (s.message && s.message.trim() ? s.message : null);

/** The frame-rate limit of the exposure loop: the longest whole stop that fits `fps` (2.5.2). */
export const rateStop = (fps: number): number => Math.floor(Math.log2(0.95 / fps));

/** What a camera that is asked for a longer exposure than the frame time runs at, about. */
const droppedFps = (stop: number): number => Math.max(1, Math.round(0.95 * 2 ** -stop));

/** `"fit"` or a whole stop, as the Select widget's text, and back. */
export const longestKey = (v: "fit" | number): string => String(v);
export const parseLongest = (s: string): "fit" | number => (s === "fit" ? "fit" : Number(s));

/**
 * The "Longest exposure" list: the frame rate first, then every whole stop of the camera's range from the shortest to
 * the longest, each with what it means. Without the rate (the camera is closed) the stops carry no meaning.
 */
export const longestOptions = (range: { min: number; max: number }, fps: number | null): Option[] => {
  const limit = fps && fps > 0 ? rateStop(fps) : null;
  const fit = limit === null ? "Fit the frame rate" : `Fit the frame rate (${exposureText(limit)} at ${Math.round(fps!)} fps)`;
  const out: Option[] = [{ value: "fit", label: fit }];
  for (let n = Math.round(range.min); n <= Math.round(range.max); n++) {
    const meaning = limit === null ? "" : n < limit ? " (sharper motion, more gain)" : n === limit ? " (the frame-rate limit)" : ` (camera drops to ≈ ${droppedFps(n)} fps)`;
    out.push({ value: longestKey(n), label: `${exposureText(n)}${meaning}` });
  }
  return out;
};

/** The tooltip of the limit tick on the exposure slider. */
export const capText = (reason: ExposureStatus["cap_reason"], fps: number): string =>
  reason === "user" ? "Your limit" : reason === "learned" ? "Learned limit" : `${Math.round(fps)} fps limit`;

/** The "Most gain" slider's value: `null` is 75 % of the range, as the backend works it out. */
export const maxGainValue = (maxGain: number | null, range: { min: number; max: number; step: number }): number => {
  if (maxGain !== null) return clamp(maxGain, range.min, range.max);
  const raw = range.min + Math.round(0.75 * (range.max - range.min));
  const step = Math.max(1, range.step);
  return clamp(range.min + Math.round((raw - range.min) / step) * step, range.min, range.max);
};

/** Where a limit sits on a slider, 0..1 (a tick and the shaded range beyond it), or `null` when it is outside the range. */
export const limitFraction = (limit: number | null | undefined, range: { min: number; max: number }): number | null =>
  limit == null || range.max <= range.min || limit < range.min || limit > range.max ? null : (limit - range.min) / (range.max - range.min);

