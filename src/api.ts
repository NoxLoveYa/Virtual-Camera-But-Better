import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { FilterCfg } from "./filters/schema";
import type { CameraInfo, OutputCfg, SourceCfg } from "./camera/schema";

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

export const api = {
  listCameras: () => invoke<CameraInfo[]>("list_cameras"),
  startEngine: (source: SourceCfg, output: OutputCfg, chain: FilterCfg[]) => invoke<void>("start_engine", { source, output, chain }),
  stopEngine: () => invoke<void>("stop_engine"),
  setSource: (source: SourceCfg) => invoke<void>("set_source", { source }),
  setOutput: (output: OutputCfg) => invoke<void>("set_output", { output }),
  setPause: (on: boolean) => invoke<void>("set_pause", { on }),
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
  launchedAtStartup: () => invoke<boolean>("launched_at_startup"),
  setCloseToTray: (on: boolean) => invoke<void>("set_close_to_tray", { on }),
  listPresets: () => invoke<string[]>("list_presets"),
  savePreset: (name: string, chain: FilterCfg[]) => invoke<void>("save_preset", { name, chain }),
  loadPreset: (name: string) => invoke<FilterCfg[]>("load_preset", { name }),
  deletePreset: (name: string) => invoke<void>("delete_preset", { name }),
  onEngineState: (cb: (s: EngineState) => void) => listen<EngineState>("engine-state", (e) => cb(e.payload)),
  onEngineError: (cb: (msg: string) => void) => listen<string>("engine-error", (e) => cb(e.payload)),
  onStats: (cb: (s: Stats) => void) => listen<Stats>("stats", (e) => cb(e.payload)),
  onDevicesChanged: (cb: () => void) => listen("devices-changed", () => cb()),
};
