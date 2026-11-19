import { Channel, invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import type { ExtensionInfo, FilterCfg } from "./filters/schema";
import type { PresetFileAny } from "./presets/preset";
import { answerFlushRequest } from "./ui/saver";
import type { PreviewPoint } from "./ui/previewFrame";
import type { AutoCfg, AutoStatus, CameraInfo, ControlInfo, ControlKey, ControlMode, Learned, OutputCfg, SourceCfg, StoredControl } from "./camera/schema";

/**
 * The record of the selected camera, as `start_engine` and `set_source` take it (see `controlsRecord`): its changed
 * controls (`mode: "app"` for Better Cam auto, `value` then the resume point), Better Cam auto's settings and what it learned.
 */
export type ControlsRecord = {
  id: string;
  name: string;
  controls: Partial<Record<ControlKey, StoredControl>>;
  auto?: AutoCfg;
  learned?: Learned;
} | null;

export type EngineStateName = "stopped" | "standby" | "live" | "reconnecting";

/**
 * `engine-state`. Reasons: standby = opening | reopening | idle; reconnecting = what is wrong; stopped = error,
 * `vcam-change` (the virtual camera is being installed, repaired or removed) or none.
 */
export interface EngineState {
  state: EngineStateName;
  reason?: string | null;
  /**
   * What kind of problem `reason` describes: `in_use`, `disconnected`, `not_found`, `privacy` or `error` while
   * reconnecting; `format` on the stop of a start that failed for want of a format.
   */
  kind?: string | null;
  /**
   * With kind `in_use`: the programs that use a camera now, most recently started first (file names such as
   * `Discord.exe`). Missing when none could be named.
   */
  apps?: string[] | null;
  /** `apps` may include a program that only reads Better Cam's virtual camera. */
  apps_maybe?: boolean;
}

/** `format-fallback`: the camera lacks the saved format; the device default runs. Both are for a sentence, e.g. `1920×1080 YUY2 60 fps`. */
export interface FormatFallback {
  asked: string;
  used: string;
  /** The picture size and the frame interval (100 ns units) that run, for the fields. */
  width: number;
  height: number;
  interval: number;
  /** The camera's native format that runs (`mjpeg`, `nv12`, ...). */
  format: string;
}

/** `engine-paused`: the pause flag changed. Sent with every change, also for the one a Start or a Stop makes. */
export interface EnginePaused {
  paused: boolean;
}

export interface Stats {
  capture: { width: number; height: number; format: string; fps: number; rate: number; decode: "hw" | "sw" | "none" };
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
  /** A sentence when the camera's data folder or frame file is encrypted (Windows EFS): the camera shows nothing whatever `state` says. `null` when there is nothing to say. */
  problem: string | null;
  /** Repair can put `problem` right (`true` too when there is none). `false` when Windows' `ProgramData` itself is encrypted: only an administrator's step helps, so the window shows no Repair button. */
  repairable: boolean;
}

/** `nvidia_status`: whether NVIDIA's Video Effects runtime is usable. `detail` says in plain words what is missing. */
export interface NvidiaStatus {
  available: boolean;
  detail: string;
  /** The runtime's own version (`0.7.6`) when it could be read. */
  version: string | null;
}

/** `nvidia_gpu`: the installed RTX GPU and the redistributable build that fits it. */
export interface NvidiaGpu {
  name: string;
  arch: "turing" | "ampere" | "ada" | "blackwell";
}

/** `nvidia-install`: `download` with bytes done and total (total 0 when unknown), then the stages `verify` and `install`. */
export interface NvidiaInstallProgress {
  stage: string;
  done: number;
  total: number;
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
  /**
   * Who sets a control: Manual (the last driven value stays), the camera's Auto flag, or Better Cam auto (exposure and
   * white balance). Resolves to the rows that changed (the exposure together with the gain). Works while the camera is
   * closed: the mode is stored and the loop starts at the next open.
   */
  setControlMode: (id: string, key: ControlKey, mode: ControlMode) => invoke<ControlInfo[]>("set_control_mode", { id, key, mode }),
  /** Better Cam auto's settings of the camera; the next decision uses them. */
  setAutoConfig: (id: string, cfg: AutoCfg) => invoke<void>("set_auto_config", { id, cfg }),
  /** Starts Better Cam auto again after a `conflict`; also clears `blocked` ("Try again": set the mode again after). */
  resumeAuto: (id: string, key: ControlKey) => invoke<void>("resume_auto", { id, key }),
  setOutput: (output: OutputCfg) => invoke<void>("set_output", { output }),
  setPause: (on: boolean) => invoke<void>("set_pause", { on }),
  /**
   * Starts, resizes (a new call replaces the stream) or stops the preview; `onFrame` gets each frame: a 28-byte header
   * (see `ui/previewFrame.ts`) + NV12 pixels.
   */
  setPreview: (on: boolean, width: number, height: number, onFrame: (frame: ArrayBuffer) => void = () => {}) => {
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = onFrame;
    return invoke<void>("set_preview", { on, width, height, channel });
  },
  previewAck: (frame: number) => invoke<void>("preview_ack", { frame }),
  /**
   * Which picture the preview shows: `null` the output (what apps get), `"input"` the camera before the chain,
   * `{ filter: id }` the picture entering that filter (on or off). The output to apps never changes. The engine puts it
   * back to `null` when that filter leaves the chain or the window is hidden; the page puts it back when its gesture ends.
   */
  setPreviewPoint: (point: PreviewPoint) => invoke<void>("set_preview_point", { point }),
  /** Opens Windows Settings on the camera privacy page (`ms-settings:privacy-webcam`). */
  openCameraSettings: () => invoke<void>("open_camera_settings"),
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
  /** Reads the GPU and the runtime folder and asks the runtime for its version; takes a moment. */
  nvidiaStatus: () => invoke<NvidiaStatus>("nvidia_status"),
  /** The installed RTX GPU, `null` when there is none. */
  nvidiaGpu: () => invoke<NvidiaGpu | null>("nvidia_gpu"),
  /**
   * Downloads NVIDIA's Video Effects runtime for `arch`, checks that NVIDIA signed it and runs it (administrator prompt).
   * Progress arrives through `onNvidiaInstall`; resolves when the installer has closed, rejects with a sentence.
   */
  installNvidiaSdk: (arch: NvidiaGpu["arch"]) => invoke<void>("install_nvidia_sdk", { arch }),
  onNvidiaInstall: (cb: (p: NvidiaInstallProgress) => void) =>
    listen<NvidiaInstallProgress>("nvidia-install", (e) => cb(e.payload)),
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
  launchedAtStartup: () => invoke<boolean>("launched_at_startup"),
  setCloseToTray: (on: boolean) => invoke<void>("set_close_to_tray", { on }),
  /** The whole chain, cheap enough for every slider step; applies before the next frame, also while the engine is stopped. */
  setChain: (chain: FilterCfg[]) => invoke<void>("set_chain", { chain }),
  listPresets: () => invoke<string[]>("list_presets"),
  /** Writes a preset file (see `presetFileOf`; a file read from disk goes back as it was) under `name`; ids are dropped, the file is replaced atomically. */
  savePresetFile: (name: string, file: PresetFileAny) => invoke<void>("save_preset", { name, preset: file }),
  /** The preset file as the backend returns it (`chain`, and `camera` and `output` when the file has them), still raw: run it through `parsePreset`. */
  loadPresetRaw: (name: string) => invoke<unknown>("load_preset", { name }),
  /** Renames a preset file. Rejects when `from` is missing or `to` is another preset; a change of letter case only is allowed. */
  renamePreset: (from: string, to: string) => invoke<void>("rename_preset", { from, to }),
  deletePreset: (name: string) => invoke<void>("delete_preset", { name }),
  /**
   * The preset file at `path` (a file dropped on the window; the object format, or the old bare array wrapped in it),
   * still raw: run it through `parsePreset`. Only reads; rejects with a sentence (over 1 MiB, not JSON, not a preset).
   */
  readPresetFile: (path: string) => invoke<unknown>("read_preset_file", { path }),
  onEngineState: (cb: (s: EngineState) => void) => listen<EngineState>("engine-state", (e) => cb(e.payload)),
  onEngineError: (cb: (msg: string) => void) => listen<string>("engine-error", (e) => cb(e.payload)),
  /** The pause flag of the engine; the window follows it (a restored engine reports the pause it was given). */
  onEnginePaused: (cb: (p: EnginePaused) => void) => listen<EnginePaused>("engine-paused", (e) => cb(e.payload)),
  /** The camera lacks the saved format and the device default was opened instead (the engine already runs with it). */
  onFormatFallback: (cb: (f: FormatFallback) => void) => listen<FormatFallback>("format-fallback", (e) => cb(e.payload)),
  /**
   * A filter failed and is bypassed until it recovers (not fatal), or a file it reads could not be loaded
   * (`could not load <file>: <reason>`). `id` is the card's id. An **empty `message`** means the problem is gone: the
   * file loaded, or its path was cleared.
   */
  onFilterError: (cb: (e: { id: string; message: string }) => void) =>
    listen<{ id: string; message: string }>("filter-error", (e) => cb(e.payload)),
  /**
   * A line for a filter's card that is not an error: "Loading model…" while NVIDIA's model loads (`filter-status`).
   * `id` is the card's id. An **empty `message`** clears the line.
   */
  onFilterStatus: (cb: (e: { id: string; message: string }) => void) =>
    listen<{ id: string; message: string }>("filter-status", (e) => cb(e.payload)),
  /** The extensions found in `%APPDATA%\dev.bettercam.app\extensions`, valid or not (see `ExtensionInfo.state`). */
  listExtensions: () => invoke<ExtensionInfo[]>("list_extensions"),
  /** Reads every extension folder again now; resolves to the list. */
  reloadExtensions: () => invoke<ExtensionInfo[]>("reload_extensions"),
  /** Switches an installed extension on or off (remembered); resolves to the list. Its cards stay and are bypassed while it is off. */
  setExtensionEnabled: (id: string, on: boolean) => invoke<ExtensionInfo[]>("set_extension_enabled", { id, on }),
  /** Opens the extensions folder in Explorer (created when missing). */
  openExtensionsFolder: () => invoke<void>("open_extensions_folder"),
  /**
   * The list of extensions changed: a folder was added, removed or edited (the app looks about once a second), or one
   * was switched on or off. Carries the whole list.
   */
  onExtensionsChanged: (cb: (list: ExtensionInfo[]) => void) => listen<ExtensionInfo[]>("extensions-changed", (e) => cb(e.payload)),
  onStats: (cb: (s: Stats) => void) => listen<Stats>("stats", (e) => cb(e.payload)),
  onDevicesChanged: (cb: () => void) => listen("devices-changed", () => cb()),
  /**
   * The app is about to exit (tray Quit, window close) or hide to the tray: `cb` saves what waits, then the page answers
   * `save-flushed`. The app waits for that answer (at most 0.4 s) before it exits.
   */
  onFlushSave: (cb: () => void) => listen("flush-save", () => answerFlushRequest(cb, () => void emit("save-flushed"))),
  /**
   * About once a second while the camera is open and a control is on Auto, up to 4 times a second while Better Cam auto
   * writes, and after the stored values were re-applied. The entries carry `mode` and the other Better Cam auto fields.
   */
  onControlsChanged: (cb: (infos: ControlInfo[]) => void) => listen<ControlInfo[]>("controls-changed", (e) => cb(e.payload)),
  /** What Better Cam auto is doing for a camera: on every state change, up to twice a second while it adjusts. */
  onAutoStatus: (cb: (s: AutoStatus) => void) => listen<AutoStatus>("auto-status", (e) => cb(e.payload)),
};
