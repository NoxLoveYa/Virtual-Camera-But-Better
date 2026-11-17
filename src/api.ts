import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { FilterCfg } from "./filters/schema";
import { presetChain, presetFile } from "./filters/chain";
import type { CameraInfo, ControlInfo, OutputCfg, SourceCfg } from "./camera/schema";

/** The changed controls of the selected camera, as `start_engine` and `set_source` take them (see `controlsRecord`). */
export type ControlsRecord = { id: string; name: string; controls: Record<string, { value: number; auto: boolean }> } | null;

export type EngineStateName = "stopped" | "standby" | "live" | "reconnecting";

/** `engine-state`. Reasons: standby = opening | reopening | idle; reconnecting = what is wrong; stopped = error or none. */
export interface EngineState {
  state: EngineStateName;
  reason?: string | null;
}

export interface Stats {
  capture: { width: number; height: number; format: string; fps: number; decode: "hw" | "sw" | "none" };
  output: { width: number; height: number; fps: number };
  gpu_ms: number;
  dropped: number;
  repeated: number;
  latency_p95_ms: number;
  consumers: number;
}

/** `missing`: not installed or not listed by Windows; `outdated`: another build than the DLL shipped with the app. */
export type VcamState = "missing" | "outdated" | "ok";

/** How an install or removal that did not fail ended: `pending_restart` takes effect after the camera service or Windows restarts. */
export type VcamResult = "done" | "pending_restart";

export interface VcamStatus {
  state: VcamState;
  /** Windows lists the camera. */
  listed: boolean;
  /** 0 or 1: an app has the camera open right now. */
  consumers: number;
}

export const api = {
  listCameras: () => invoke<CameraInfo[]>("list_cameras"),
  startEngine: (source: SourceCfg, output: OutputCfg, chain: FilterCfg[], controls: ControlsRecord = null) =>
    invoke<void>("start_engine", { source, output, chain, controls }),
  stopEngine: () => invoke<void>("stop_engine"),
  setSource: (source: SourceCfg, controls: ControlsRecord = null) => invoke<void>("set_source", { source, controls }),
  /** What the camera supports, with its own ranges; opens the device for a moment when the engine has no camera open. */
  getCameraControls: (id: string) => invoke<ControlInfo[]>("get_camera_controls", { id }),
  /** A manual value, or Auto on; resolves to the control as the device reports it. */
  setCameraControl: (id: string, key: string, value: number, auto: boolean) =>
    invoke<ControlInfo>("set_camera_control", { id, key, value, auto }),
  resetCameraControls: (id: string) => invoke<ControlInfo[]>("reset_camera_controls", { id }),
  setOutput: (output: OutputCfg) => invoke<void>("set_output", { output }),
  setPause: (on: boolean) => invoke<void>("set_pause", { on }),
  /** Starts, resizes (a new call replaces the stream) or stops the preview; `onFrame` gets each frame: 16-byte header + RGBA pixels. */
  setPreview: (on: boolean, width: number, height: number, onFrame: (frame: ArrayBuffer) => void = () => {}) => {
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = onFrame;
    return invoke<void>("set_preview", { on, width, height, channel });
  },
  previewAck: (frame: number) => invoke<void>("preview_ack", { frame }),
  /** No elevation; takes a moment (Media Foundation enumeration). */
  vcamStatus: () => invoke<VcamStatus>("vcam_status"),
  /**
   * Stops the engine, then installs, updates or repairs the virtual camera: one UAC prompt. Resolves when the helper
   * has exited, however long the prompt stays open (a second call meanwhile is refused): `"done"`, or
   * `"pending_restart"` when it takes effect only after the camera service or Windows restarts. Rejects with a
   * sentence when declined or failed.
   */
  vcamInstall: () => invoke<VcamResult>("vcam_install"),
  /** Stops the engine, then removes the virtual camera: one UAC prompt. Resolves and rejects like `vcamInstall`. */
  vcamUninstall: () => invoke<VcamResult>("vcam_uninstall"),
  /** The native "open file" dialog: the full path of the chosen file, `null` when cancelled. The page keeps running while it is open. */
  pickFile: (kind: "image" | "lut") => invoke<string | null>("pick_file", { kind }),
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
  launchedAtStartup: () => invoke<boolean>("launched_at_startup"),
  setCloseToTray: (on: boolean) => invoke<void>("set_close_to_tray", { on }),
  /** The whole chain, cheap enough for every slider step; applies before the next frame, also while the engine is stopped. */
  setChain: (chain: FilterCfg[]) => invoke<void>("set_chain", { chain }),
  listPresets: () => invoke<string[]>("list_presets"),
  /** Writes the preset file of plan 5.4 (no ids). */
  savePreset: (name: string, chain: FilterCfg[]) => invoke<void>("save_preset", { name, preset: presetFile(chain) }),
  /** The chain of the preset file (the object, or the old bare array), normalized, with fresh ids. */
  loadPreset: (name: string) => invoke<unknown>("load_preset", { name }).then(presetChain),
  deletePreset: (name: string) => invoke<void>("delete_preset", { name }),
  onEngineState: (cb: (s: EngineState) => void) => listen<EngineState>("engine-state", (e) => cb(e.payload)),
  onEngineError: (cb: (msg: string) => void) => listen<string>("engine-error", (e) => cb(e.payload)),
  /**
   * A filter failed and is bypassed until it recovers (not fatal), or a file it reads could not be loaded
   * (`could not load <file>: <reason>`). `id` is the card's id. An **empty `message`** means the problem is gone: the
   * file loaded, or its path was cleared.
   */
  onFilterError: (cb: (e: { id: string; message: string }) => void) =>
    listen<{ id: string; message: string }>("filter-error", (e) => cb(e.payload)),
  onStats: (cb: (s: Stats) => void) => listen<Stats>("stats", (e) => cb(e.payload)),
  onDevicesChanged: (cb: () => void) => listen("devices-changed", () => cb()),
  /** About once a second while the camera is open and a control is on Auto, and after the stored values were re-applied. */
  onControlsChanged: (cb: (infos: ControlInfo[]) => void) => listen<ControlInfo[]>("controls-changed", (e) => cb(e.payload)),
};
