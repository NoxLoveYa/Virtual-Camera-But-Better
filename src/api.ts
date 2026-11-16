import { invoke } from "@tauri-apps/api/core";
import type { FilterCfg } from "./filters/schema";

export const api = {
  getAutostart: () => invoke<boolean>("get_autostart"),
  setAutostart: (on: boolean) => invoke<void>("set_autostart", { on }),
  launchedAtStartup: () => invoke<boolean>("launched_at_startup"),
  setCloseToTray: (on: boolean) => invoke<void>("set_close_to_tray", { on }),
  listPresets: () => invoke<string[]>("list_presets"),
  savePreset: (name: string, chain: FilterCfg[]) => invoke<void>("save_preset", { name, chain }),
  loadPreset: (name: string) => invoke<FilterCfg[]>("load_preset", { name }),
  deletePreset: (name: string) => invoke<void>("delete_preset", { name }),
};
