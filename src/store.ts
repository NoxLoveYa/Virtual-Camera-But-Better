import { create } from "zustand";
import { api, type EngineState, type Stats } from "./api";
import { FILTERS, defaultChain, makeFilter, type FilterCfg, type FilterKind, type ParamValue } from "./filters/schema";
import { normalizeChain, signature } from "./filters/chain";
import {
  DEFAULT_OUTPUT,
  DEFAULT_SOURCE,
  normalizeOutput,
  normalizeSource,
  repairSource,
  type CameraInfo,
  type OutputCfg,
  type SourceCfg,
} from "./camera/schema";

const STORAGE_KEY = "better-cam";

interface Saved {
  source: SourceCfg;
  output: OutputCfg;
  chain: FilterCfg[];
  closeToTray: boolean;
  /** Whether the "launch on startup" default has been applied once; after that the OS entry is the truth. */
  autostartDefaulted: boolean;
  /** The preset the chain was last loaded from or saved as, and what it looked like then (see `signature`). */
  activePreset: string | null;
  presetBaseline: string | null;
  /** Whether the engine was running when the app last ran, so the next launch starts it again. */
  wasRunning: boolean;
}

/** What "Undo" restores after loading a preset or resetting. */
interface Snapshot {
  chain: FilterCfg[];
  activePreset: string | null;
  presetBaseline: string | null;
}

/** The engine's state as the last `engine-state` event said it. Pause is not part of it (see `paused`). */
export interface EngineInfo {
  state: EngineState["state"];
  reason: string | null;
}

const STOPPED: EngineInfo = { state: "stopped", reason: null };
const isRunning = (e: EngineInfo) => e.state !== "stopped";

const loadSaved = (): Saved => {
  const fresh: Saved = {
    source: DEFAULT_SOURCE,
    output: DEFAULT_OUTPUT,
    chain: defaultChain(),
    closeToTray: false,
    autostartDefaulted: false,
    activePreset: null,
    presetBaseline: null,
    wasRunning: false,
  };
  try {
    const s = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "");
    if (s && typeof s === "object") {
      return {
        source: normalizeSource(s.source),
        output: normalizeOutput(s.output),
        chain: Array.isArray(s.chain) ? normalizeChain(s.chain) : fresh.chain,
        closeToTray: s.closeToTray === true,
        autostartDefaulted: s.autostartDefaulted === true,
        activePreset: typeof s.activePreset === "string" ? s.activePreset : null,
        presetBaseline: typeof s.presetBaseline === "string" ? s.presetBaseline : null,
        wasRunning: s.wasRunning === true,
      };
    }
  } catch {
    /* first run or corrupt data */
  }
  return fresh;
};

/** The 4 Hz stats live in their own store, so only the footer re-renders for them. */
export const useStats = create<{ stats: Stats | null }>(() => ({ stats: null }));

interface State extends Saved {
  autostart: boolean;
  /** `null` until the first `list_cameras` answer. */
  cameras: CameraInfo[] | null;
  engine: EngineInfo;
  /** Pause is not an engine state and no event reports it: the store keeps the flag; Start and Stop clear it. */
  paused: boolean;
  error: string | null;
  notice: string | null;
  /** Bumped on every toast so repeating the same message restarts its timer. */
  noticeSeq: number;
  undo: Snapshot | null;
  collapsed: Record<string, boolean>;
  presets: string[];

  init: () => Promise<void>;
  refreshCameras: () => Promise<void>;
  /** Changes camera settings (repaired against the device), persists them and, while running, sends them at once. */
  patchSource: (patch: Partial<SourceCfg>) => void;
  patchOutput: (patch: Partial<OutputCfg>) => void;
  /** Like `patchOutput({ background_color })`, but the running engine hears of it at most every ~100 ms (colour pickers). */
  setBackground: (abgr: number) => void;
  toggleRun: () => Promise<void>;
  togglePause: () => void;
  setAutostart: (on: boolean) => Promise<void>;
  setCloseToTray: (on: boolean) => void;
  dismissError: () => void;
  dismissNotice: () => void;
  undoChain: () => void;

  addFilter: (kind: FilterKind) => void;
  removeFilter: (id: string) => void;
  moveFilter: (from: number, to: number) => void;
  toggleFilter: (id: string) => void;
  setParam: (id: string, key: string, value: ParamValue) => void;
  toggleCollapsed: (id: string) => void;
  setAllCollapsed: (on: boolean) => void;

  refreshPresets: () => Promise<void>;
  /** Saves the current chain under `name` (replacing any preset with that name) and makes it the active preset. */
  savePreset: (name: string) => Promise<void>;
  saveActivePreset: () => Promise<void>;
  loadPreset: (name: string) => Promise<void>;
  deletePreset: (name: string) => Promise<void>;
  resetChain: () => void;
}

export const useStore = create<State>((set, get) => {
  const persist = () => {
    const { source, output, chain, closeToTray, autostartDefaulted, activePreset, presetBaseline, wasRunning } = get();
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ source, output, chain, closeToTray, autostartDefaulted, activePreset, presetBaseline, wasRunning }),
    );
  };

  const setChain = (chain: FilterCfg[]) => {
    set({ chain });
    persist();
  };

  const fail = (e: unknown) => set({ error: String(e) });

  /** Shows a toast, optionally with an Undo that restores `undo`. */
  const say = (notice: string, undo: Snapshot | null = null) => set((s) => ({ notice, undo, noticeSeq: s.noticeSeq + 1 }));

  const snapshot = (): Snapshot => {
    const { chain, activePreset, presetBaseline } = get();
    return { chain, activePreset, presetBaseline };
  };

  /** Swaps in a whole chain (preset load, reset, undo) and records which preset it came from. */
  const replaceChain = (chain: FilterCfg[], activePreset: string | null, presetBaseline: string | null) => {
    set({ activePreset, presetBaseline });
    setChain(chain);
  };

  const running = () => isRunning(get().engine);
  const pushSource = () => {
    if (running()) api.setSource(get().source).catch(fail);
  };
  const pushOutput = () => {
    if (running()) api.setOutput(get().output).catch(fail);
  };

  /** One `list_cameras` at a time; a request that arrives meanwhile makes the running one go round again. */
  let listing: Promise<void> | null = null;
  let relist = false;
  const loadCameras = async () => {
    const cameras = await api.listCameras();
    set({ cameras });
    const prev = get().source;
    const next = repairSource(prev, cameras);
    if (JSON.stringify(next) === JSON.stringify(prev)) return;
    set({ source: next });
    persist();
    pushSource();
    // The very first pick of a camera is no news; later changes are the camera not offering what was saved.
    if (prev.video_device_id) say("Camera settings adjusted to what the camera offers");
  };

  let backgroundTimer: ReturnType<typeof setTimeout> | undefined;

  /** Starts the engine with the saved state; false when the call was rejected (the error banner says why). */
  const begin = async (): Promise<boolean> => {
    const { source, output, chain } = get();
    try {
      await api.startEngine(source, output, chain);
      // The events may already have said more than "standby"; only fill in when they have not.
      if (!running()) set({ engine: { state: "standby", reason: "opening" } });
      set({ error: null, paused: false, wasRunning: true });
      persist();
      return true;
    } catch (e) {
      fail(e);
      return false;
    }
  };

  const finish = async () => {
    try {
      await api.stopEngine();
      set({ engine: STOPPED, paused: false, wasRunning: false });
      useStats.setState({ stats: null });
      persist();
    } catch (e) {
      fail(e);
    }
  };

  return {
    ...loadSaved(),
    autostart: false,
    cameras: null,
    engine: STOPPED,
    paused: false,
    error: null,
    notice: null,
    noticeSeq: 0,
    undo: null,
    collapsed: {},
    presets: [],

    init: async () => {
      await api.onEngineState((e) => {
        set({ engine: { state: e.state, reason: e.reason ?? null }, ...(e.state === "stopped" && { paused: false }) });
        if (e.state === "stopped") useStats.setState({ stats: null });
      });
      await api.onEngineError((error) => {
        api.stopEngine().catch(() => {});
        set({ error, engine: STOPPED, paused: false, wasRunning: false });
        useStats.setState({ stats: null });
        persist();
      });
      await api.onStats((stats) => useStats.setState({ stats }));
      await api.onDevicesChanged(() => void get().refreshCameras());
      await Promise.all([get().refreshCameras(), get().refreshPresets()]).catch(fail);

      // Work out which saved preset (if any) the restored chain still matches, so the toolbar shows the right name.
      const { presets } = get();
      if (get().activePreset && !presets.includes(get().activePreset!)) set({ activePreset: null, presetBaseline: null });
      if (!get().activePreset) {
        const sig = signature(get().chain);
        for (const name of presets) {
          const saved = await api.loadPreset(name).then(normalizeChain).catch(() => null);
          if (saved && signature(saved) === sig) {
            set({ activePreset: name, presetBaseline: sig });
            break;
          }
        }
      }
      persist();

      try {
        if (!get().autostartDefaulted) {
          await api.setAutostart(true);
          set({ autostartDefaulted: true });
          persist();
        }
        set({ autostart: await api.getAutostart() });
        await api.setCloseToTray(get().closeToTray);
      } catch (e) {
        fail(e);
      }

      // Both a login launch and a manual one start again when the engine was running at the last exit. Right after
      // login the engine may not be ready yet, so a login launch keeps trying for a while.
      if (get().wasRunning) {
        const login = await api.launchedAtStartup().catch(() => false);
        for (let i = 0; i < (login ? 10 : 1) && !running(); i++) {
          if (!(await begin()) && login) await new Promise((r) => setTimeout(r, 3000));
        }
      }
    },

    refreshCameras: () => {
      if (listing) {
        relist = true;
        return listing;
      }
      listing = (async () => {
        try {
          do {
            relist = false;
            await loadCameras();
          } while (relist);
        } catch (e) {
          set((s) => ({ cameras: s.cameras ?? [] }));
          fail(e);
        }
      })().finally(() => {
        listing = null;
      });
      return listing;
    },

    patchSource: (patch) => {
      const prev = get().source;
      const next = { ...prev, ...patch };
      const { cameras } = get();
      const fixed = cameras ? repairSource(next, cameras) : next;
      if (JSON.stringify(fixed) === JSON.stringify(prev)) return;
      set({ source: fixed });
      persist();
      pushSource();
    },

    patchOutput: (patch) => {
      set({ output: { ...get().output, ...patch } });
      persist();
      clearTimeout(backgroundTimer);
      pushOutput();
    },

    setBackground: (abgr) => {
      set({ output: { ...get().output, background_color: abgr } });
      persist();
      clearTimeout(backgroundTimer);
      backgroundTimer = setTimeout(pushOutput, 100);
    },

    toggleRun: async () => {
      if (running()) await finish();
      else await begin();
    },

    togglePause: () => {
      if (!running()) return;
      const paused = !get().paused;
      set({ paused });
      api.setPause(paused).catch(fail);
    },

    setAutostart: async (on) => {
      try {
        await api.setAutostart(on);
        set({ autostart: on });
      } catch (e) {
        fail(e);
      }
    },

    setCloseToTray: (closeToTray) => {
      set({ closeToTray });
      persist();
      api.setCloseToTray(closeToTray).catch(fail);
    },

    dismissError: () => set({ error: null, notice: null, undo: null }),
    dismissNotice: () => set({ notice: null, undo: null }),
    undoChain: () => {
      const u = get().undo;
      if (!u) return;
      replaceChain(u.chain, u.activePreset, u.presetBaseline);
      say("Undone");
    },

    addFilter: (kind) => {
      const prev = snapshot();
      setChain([...prev.chain, makeFilter(kind)]);
      say(`Added ${FILTERS[kind].label} as step ${prev.chain.length + 1}`, prev);
    },
    removeFilter: (id) => {
      const prev = snapshot();
      const gone = prev.chain.find((f) => f.id === id);
      setChain(prev.chain.filter((f) => f.id !== id));
      if (gone) say(`Removed ${FILTERS[gone.kind].label}`, prev);
    },
    moveFilter: (from, to) => {
      const chain = [...get().chain];
      chain.splice(to, 0, chain.splice(from, 1)[0]);
      setChain(chain);
    },
    toggleFilter: (id) => setChain(get().chain.map((f) => (f.id === id ? { ...f, enabled: !f.enabled } : f))),
    setParam: (id, key, value) =>
      setChain(get().chain.map((f) => (f.id === id ? { ...f, params: { ...f.params, [key]: value } } : f))),

    toggleCollapsed: (id) => set((s) => ({ collapsed: { ...s.collapsed, [id]: !s.collapsed[id] } })),
    setAllCollapsed: (on) => set((s) => ({ collapsed: Object.fromEntries(s.chain.map((f) => [f.id, on])) })),

    refreshPresets: async () => set({ presets: await api.listPresets() }),
    savePreset: async (name) => {
      const { chain } = get();
      try {
        await api.savePreset(name, chain);
        set({ activePreset: name, presetBaseline: signature(chain) });
        persist();
        await get().refreshPresets();
        say(`Saved “${name}”`);
      } catch (e) {
        fail(e);
      }
    },
    saveActivePreset: async () => {
      const name = get().activePreset;
      if (name) await get().savePreset(name);
    },
    loadPreset: async (name) => {
      try {
        const chain = normalizeChain(await api.loadPreset(name));
        const prev = snapshot();
        replaceChain(chain, name, signature(chain));
        say(`Loaded “${name}”`, signature(prev.chain) === signature(chain) ? null : prev);
      } catch (e) {
        fail(e);
      }
    },
    deletePreset: async (name) => {
      try {
        await api.deletePreset(name);
        if (get().activePreset === name) {
          set({ activePreset: null, presetBaseline: null });
          persist();
        }
        await get().refreshPresets();
        say(`Deleted “${name}”`);
      } catch (e) {
        fail(e);
      }
    },
    resetChain: () => {
      const prev = snapshot();
      replaceChain(defaultChain(), null, null);
      say("Reset to the default chain", prev);
    },
  };
});
