/**
 * The preset file, version 2 (docs/LAYOUT-INSPECTOR.md section 3): the filter chain, the camera (device, capture
 * settings, changed controls, Better Cam auto's settings) and the output. Pure: nothing here touches React, Tauri or
 * the DOM, so a script can import it. The store (P2) turns a parsed preset into engine calls; `planControls` is the
 * part of that which needs no engine.
 */
import type { ExtensionInfo, FilterCfg } from "../filters/schema";
import { normalizeChain, signature } from "../filters/chain";
import {
  CONTROL_KEYS,
  DEFAULT_AUTO,
  DEFAULT_OUTPUT,
  DEFAULT_SOURCE,
  findCamera,
  largestWide,
  linkKey,
  modeOf,
  normalizeAutoCfg,
  normalizeControls,
  normalizeOutput,
  normalizeSource,
  vidPid,
  type AutoCfg,
  type CameraInfo,
  type ControlInfo,
  type ControlKey,
  type ControlMode,
  type OutputCfg,
  type SourceCfg,
  type StoredControl,
} from "../camera/schema";

/** A chain entry as the file holds it: no id. */
export type PresetEntry = Pick<FilterCfg, "kind" | "enabled" | "params">;

export interface PresetCamera {
  /** The camera the preset was made with; `null` when none was selected. */
  device: { id: string; name: string } | null;
  capture: Omit<SourceCfg, "video_device_id">;
  /** Only the controls that differ from the device's default state. */
  controls: Partial<Record<ControlKey, StoredControl>>;
  auto: AutoCfg;
}

/** What a preset sets, as the store holds it (ids kept: this is also the Undo snapshot). */
export interface PresetState {
  chain: FilterCfg[];
  camera: PresetCamera;
  output: OutputCfg;
}

export interface PresetFileV2 {
  app: "better-cam";
  version: 2;
  chain: PresetEntry[];
  camera: PresetCamera;
  output: OutputCfg;
}

/** A file read from disk: v1 files have no camera and no output. */
export interface ParsedPreset {
  version: number;
  chain: FilterCfg[];
  camera: PresetCamera | null;
  output: OutputCfg | null;
}

const isObject = (v: unknown): v is Record<string, unknown> => !!v && typeof v === "object" && !Array.isArray(v);

/** The file of a state: the chain without ids, the other sections as they are. */
export const presetFileOf = (state: PresetState): PresetFileV2 => ({
  app: "better-cam",
  version: 2,
  chain: state.chain.map(({ kind, enabled, params }) => ({ kind, enabled, params: { ...params } })),
  camera: {
    device: state.camera.device ? { ...state.camera.device } : null,
    capture: { ...state.camera.capture },
    controls: Object.fromEntries(Object.entries(state.camera.controls).map(([k, c]) => [k, { ...c }])),
    auto: { exposure: { ...state.camera.auto.exposure }, white_balance: { ...state.camera.auto.white_balance } },
  },
  output: { ...state.output },
});

const withoutDevice = ({ video_device_id: _, ...capture }: SourceCfg): PresetCamera["capture"] => capture;

/** A `camera` section read from a file: `null` unless it is an object with a `capture` object. Each bad field is its default. */
const parseCamera = (raw: unknown): PresetCamera | null => {
  if (!isObject(raw) || !isObject(raw.capture)) return null;
  const d = raw.device;
  const device = isObject(d) && typeof d.id === "string" && d.id !== "" ? { id: d.id, name: typeof d.name === "string" ? d.name : "" } : null;
  // The per-control checks of the store's own reader, applied to a record of one camera.
  const controls = normalizeControls({ preset: { name: "", values: isObject(raw.controls) ? raw.controls : {} } }).preset?.values ?? {};
  return {
    device,
    capture: withoutDevice(normalizeSource(raw.capture)),
    controls,
    auto: normalizeAutoCfg(raw.auto) ?? normalizeAutoCfg(DEFAULT_AUTO)!,
  };
};

/**
 * A bare array, `{chain}`, a v1 object or a v2 object (a newer version is read for the sections it has too). Every
 * section is normalised. Throws "This is not a Better Cam preset" when there is no chain list.
 */
export function parsePreset(raw: unknown, extensions: readonly ExtensionInfo[]): ParsedPreset {
  const obj = isObject(raw) ? raw : null;
  const list = Array.isArray(raw) ? raw : obj?.chain;
  if (!Array.isArray(list)) throw new Error("This is not a Better Cam preset");
  const v = obj?.version;
  return {
    version: typeof v === "number" && Number.isInteger(v) && v >= 1 ? v : 1,
    chain: normalizeChain(list, extensions),
    camera: obj ? parseCamera(obj.camera) : null,
    output: obj && isObject(obj.output) ? normalizeOutput(obj.output) : null,
  };
}

/** The camera as a signature part: the same camera on another USB port (another link, same vid/pid and name) is the same. */
const deviceKey = (d: PresetCamera["device"]) => (d ? [vidPid(d.id) ?? linkKey(d.id), d.name.toLowerCase()] : null);

/**
 * Fingerprint for "unsaved changes" (3.6). Ids are ignored; the value of a Better Cam auto control is ignored (it is the
 * resume point, which moves while the loop runs); `resolution` and `frame_interval` are ignored under Device default.
 * Starts with `v2:` so a baseline saved by the filters-only build (a bare chain fingerprint) is told apart.
 */
export function presetSignature(state: PresetState): string {
  const c = state.camera.capture;
  const preferred = c.res_type === "preferred";
  const capture = [
    c.res_type,
    preferred ? null : c.resolution,
    preferred ? null : c.frame_interval,
    c.video_format,
    c.color_space,
    c.color_range,
    c.buffering,
    c.flip_vertically,
    c.autorotation,
    c.hw_decode,
    c.deactivate_when_not_showing,
  ];
  const controls = CONTROL_KEYS.flatMap((k) => {
    const s = state.camera.controls[k];
    return s ? [[k, s.mode === "app" ? "app" : s.value, s.mode === "app" ? true : s.auto]] : [];
  });
  const a = normalizeAutoCfg(state.camera.auto) ?? DEFAULT_AUTO;
  const auto = [a.exposure.target, a.exposure.longest, a.exposure.max_gain, a.exposure.speed, a.white_balance.warmth, a.white_balance.speed];
  const o = normalizeOutput(state.output);
  const output = [o.resolution, o.fps, o.fit, o.background_color, o.extra_sizes];
  return `v2:${JSON.stringify([signature(state.chain), deviceKey(state.camera.device), capture, controls, auto, output])}`;
}

/** The defaults for a camera: empty chain, DEFAULT_SOURCE (largest 16:9 size), no controls, DEFAULT_AUTO, DEFAULT_OUTPUT. */
export function defaultState(cam: CameraInfo | null): PresetState {
  return {
    chain: [],
    camera: {
      device: cam ? { id: cam.id, name: cam.name } : null,
      capture: { ...withoutDevice(DEFAULT_SOURCE), resolution: (cam && largestWide(cam)) || DEFAULT_SOURCE.resolution },
      controls: {},
      auto: normalizeAutoCfg(DEFAULT_AUTO)!,
    },
    output: { ...DEFAULT_OUTPUT },
  };
}

/**
 * The camera a preset names, among the connected ones: `findCamera` (the same link, else the only one with that
 * vid/pid), else the only one with the same name (and the same vid/pid, or none on both sides). A preset never makes
 * the app open a device that is not in `cams`.
 */
export function presetCamera(cams: CameraInfo[], device: PresetCamera["device"]): CameraInfo | null {
  if (!device) return null;
  const hit = findCamera(cams, device.id);
  if (hit) return hit;
  const vp = vidPid(device.id);
  const name = device.name.toLowerCase();
  const same = cams.filter((c) => vidPid(c.id) === vp && c.name.toLowerCase() === name);
  return same.length === 1 ? same[0] : null;
}

// ---------------------------------------------------------------------------------------------------------------
// The control plan (3.4)

export type ControlStep =
  | { key: ControlKey; op: "mode"; mode: ControlMode }
  | { key: ControlKey; op: "set"; value: number; auto: boolean };

export interface ControlPlan {
  /** Leave Better Cam auto where the target is not `app`. */
  a: ControlStep[];
  /** Every other control, in `CONTROL_KEYS` order. */
  b: ControlStep[];
  /** Engage Better Cam auto, exposure first, then white balance. */
  c: ControlStep[];
  /** Keys of the target that were not applied, with the reason (a note for the user). */
  skipped: { key: ControlKey; why: string }[];
  /**
   * Keys (exposure, white balance) the target wants in Better Cam auto while the camera offers none: they apply as
   * Camera auto when the camera has Auto, else as Manual at the stored value.
   */
  downgraded: ControlKey[];
}

const clamp = (n: number, min: number, max: number) => Math.min(max, Math.max(min, Math.round(n)));

const LOOPS = ["exposure", "white_balance"] as const;

/**
 * The writes that bring the camera from `infos` (as it reports now) to `target`. For each control the camera reports
 * the wanted state is `target[key]`, else the device's default state (Auto where it has Auto, else its default value).
 * Only what differs is written; values are clamped to the camera's range.
 */
export function planControls(infos: ControlInfo[], target: PresetCamera["controls"]): ControlPlan {
  const plan: ControlPlan = { a: [], b: [], c: [], skipped: [], downgraded: [] };
  const byKey = new Map(infos.map((i) => [i.key, i]));

  for (const key of CONTROL_KEYS) if (target[key] && !byKey.has(key)) plan.skipped.push({ key, why: "not on this camera" });

  // The wanted mode of every control the camera reports. The gain is Better Cam auto only with the exposure.
  const offered = (key: ControlKey) => byKey.get(key)?.app != null;
  const exposureApp = target.exposure?.mode === "app" && offered("exposure");
  const want = new Map<ControlKey, { mode: ControlMode; value: number }>();
  for (const info of infos) {
    const t = target[info.key];
    const value = clamp(t ? t.value : info.def, info.min, info.max);
    let mode: ControlMode;
    if (!t) mode = info.can_auto ? "camera" : "manual";
    else if (t.mode === "app") {
      const loop = info.key === "exposure" || info.key === "white_balance";
      const ok = info.key === "gain" ? exposureApp : loop && offered(info.key);
      if (!ok && loop) plan.downgraded.push(info.key);
      mode = ok ? "app" : info.can_auto ? "camera" : "manual";
    } else mode = t.auto && info.can_auto ? "camera" : "manual";
    want.set(info.key, { mode, value });
  }

  // What the device state will be once phase A has run: a loop that is handed back leaves Auto off, or on for Camera auto.
  const after = new Map<ControlKey, { auto: boolean; value: number }>(infos.map((i) => [i.key, { auto: i.auto, value: i.value }]));

  for (const key of LOOPS) {
    const info = byKey.get(key);
    const w = want.get(key);
    if (!info || !w || modeOf(info) !== "app" || w.mode === "app") continue;
    plan.a.push({ key, op: "mode", mode: w.mode });
    after.set(key, { auto: w.mode === "camera", value: info.value });
    const gain = byKey.get("gain");
    if (key === "exposure" && gain && modeOf(gain) === "app") after.set("gain", { auto: false, value: gain.value });
  }

  for (const key of CONTROL_KEYS) {
    const info = byKey.get(key);
    const w = want.get(key);
    if (!info || !w || w.mode === "app") continue;
    const now = after.get(key)!;
    if (w.mode === "camera") {
      if (!now.auto) plan.b.push({ key, op: "set", value: w.value, auto: true });
    } else if (now.auto || now.value !== w.value) {
      if (info.can_manual) plan.b.push({ key, op: "set", value: w.value, auto: false });
      else plan.skipped.push({ key, why: "no manual mode" });
    }
  }

  for (const key of LOOPS) {
    const info = byKey.get(key);
    if (info && want.get(key)?.mode === "app" && modeOf(info) !== "app") plan.c.push({ key, op: "mode", mode: "app" });
  }
  return plan;
}
