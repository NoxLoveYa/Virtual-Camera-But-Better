import { create } from "zustand";
import { api, type EngineState, type NvidiaGpu, type NvidiaInstallProgress, type NvidiaStatus, type Stats, type VcamStatus } from "./api";
import { FILTERS, defaultChain, makeFilter, type FilterCfg, type FilterKind, type ParamValue } from "./filters/schema";
import { normalizeChain, signature } from "./filters/chain";
import {
  DEFAULT_OUTPUT,
  DEFAULT_SOURCE,
  normalizeControls,
  normalizeOutput,
  normalizeSource,
  repairSource,
  controlsRecord,
  findCamera,
  forgetControls,
  rememberControl,
  sortControls,
  type CameraInfo,
  type ControlInfo,
  type ControlKey,
  type OutputCfg,
  type SourceCfg,
  type StoredControls,
} from "./camera/schema";

const STORAGE_KEY = "better-cam";

/** `npm run tauri dev` (the Vite dev server); false in a release build. The project has no `vite/client` types. */
const IS_DEV_BUILD = (import.meta as unknown as { env?: { DEV?: boolean } }).env?.DEV === true;

interface Saved {
  source: SourceCfg;
  output: OutputCfg;
  /** Only the camera controls the user changed, per camera. Re-applied by the engine on every open. */
  controls: StoredControls;
  chain: FilterCfg[];
  /** Filter cards folded in the chain, by filter id (ids are saved with the chain, so this survives a restart). */
  collapsed: Record<string, boolean>;
  closeToTray: boolean;
  /** Whether the "launch on startup" default has been applied once; after that the OS entry is the truth. */
  autostartDefaulted: boolean;
  /** Set once the NVIDIA install prompt was closed, so it does not come back on every launch. */
  nvidiaPromptDismissed: boolean;
  /** The preset the chain was last loaded from or saved as, and what it looked like then (see `signature`). */
  activePreset: string | null;
  presetBaseline: string | null;
  /** Whether the engine was running when the app last ran, so the next launch starts it again. */
  wasRunning: boolean;
  /** The live preview in the window; off saves the IPC and the CPU it costs. */
  previewOn: boolean;
}

/** What "Undo" restores after loading a preset or resetting. */
interface Snapshot {
  chain: FilterCfg[];
  activePreset: string | null;
  presetBaseline: string | null;
}

/** What the virtual camera is being asked to do (the button that was pressed). */
export type VcamAction = "install" | "update" | "repair" | "remove";

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
    controls: {},
    chain: defaultChain(),
    collapsed: {},
    closeToTray: false,
    autostartDefaulted: false,
    nvidiaPromptDismissed: false,
    activePreset: null,
    presetBaseline: null,
    wasRunning: false,
    previewOn: true,
  };
  try {
    const s = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "");
    if (s && typeof s === "object") {
      const chain = Array.isArray(s.chain) ? normalizeChain(s.chain) : fresh.chain;
      const folded = s.collapsed && typeof s.collapsed === "object" ? (s.collapsed as Record<string, unknown>) : {};
      return {
        source: normalizeSource(s.source),
        output: normalizeOutput(s.output),
        controls: normalizeControls(s.controls),
        chain,
        collapsed: Object.fromEntries(chain.filter((f) => folded[f.id] === true).map((f) => [f.id, true])),
        closeToTray: s.closeToTray === true,
        autostartDefaulted: s.autostartDefaulted === true,
        nvidiaPromptDismissed: s.nvidiaPromptDismissed === true,
        activePreset: typeof s.activePreset === "string" ? s.activePreset : null,
        presetBaseline: typeof s.presetBaseline === "string" ? s.presetBaseline : null,
        wasRunning: s.wasRunning === true,
        previewOn: s.previewOn !== false,
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
  /** What the engine said when a filter failed, by filter id; gone when that filter's entry changes. */
  filterErrors: Record<string, string>;
  /** Lines the engine shows on a card that are not errors ("Loading model…"), by filter id; gone when the engine stops or the card is. */
  filterStatus: Record<string, string>;
  presets: string[];
  /** What the selected camera's controls say now; `null` until they were read. */
  controlInfos: ControlInfo[] | null;
  /** The camera `controlInfos` belong to. */
  controlsFor: string | null;
  controlsError: string | null;
  /** Reset all is running. */
  controlsResetting: boolean;
  /** The virtual camera as `vcam_status` last said; `null` until the first answer. */
  vcam: VcamStatus | null;
  /** The action whose administrator prompt or work is running; the buttons are disabled meanwhile. */
  vcamBusy: VcamAction | null;
  /** Bumped when the virtual camera was installed, removed or changed state: the engine behind the preview is new. */
  vcamEpoch: number;
  /** NVIDIA's Video Effects runtime as `nvidia_status` last said; `null` until the first answer. */
  nvidia: NvidiaStatus | null;
  /** The installed RTX GPU; `null` when there is none (or until it was read). Filter cards offer the install only with one. */
  gpu: NvidiaGpu | null;
  /** `gpu` and `nvidia` have been read once, so a `null` GPU means "none" and not "not asked yet". */
  gpuRead: boolean;
  /** The install dialog is open. */
  nvidiaPrompt: boolean;
  /** The install is running; `stage` is `download`, `verify` or `install`. */
  nvidiaInstall: NvidiaInstallProgress | null;
  nvidiaError: string | null;

  init: () => Promise<void>;
  /** Reads the GPU (until one was found) and `nvidia_status`. Cheap enough for every window focus. */
  refreshNvidia: () => Promise<void>;
  openNvidiaPrompt: () => void;
  /** Closing the dialog is remembered: it does not open by itself again. */
  closeNvidiaPrompt: () => void;
  /** Downloads and runs NVIDIA's installer (administrator prompt), then reads the status again. */
  installNvidia: () => Promise<void>;
  /** Reads `vcam_status` (skipped while an action runs: its end reads it again). */
  refreshVcam: () => Promise<void>;
  /** Install, update or repair: one UAC prompt. A refusal or failure shows a toast; the banner stays. */
  runVcam: (action: Exclude<VcamAction, "remove">) => Promise<void>;
  /** Stops the engine and removes the virtual camera: one UAC prompt. */
  removeVcam: () => Promise<void>;
  refreshCameras: () => Promise<void>;
  /** Reads the selected camera's controls (from the open camera, else by opening the device for a moment). */
  refreshControls: () => Promise<void>;
  /** A manual value (the control leaves Auto), or the Auto flag alone. Applied at once, at most ~20 writes a second per control. */
  setControl: (key: ControlKey, patch: { value?: number; auto?: boolean }) => void;
  /** One control back to the device's default (Auto on where it has Auto). */
  resetControl: (key: ControlKey) => void;
  resetAllControls: () => Promise<void>;
  /** Changes camera settings (repaired against the device), persists them and, while running, sends them at once. */
  patchSource: (patch: Partial<SourceCfg>) => void;
  patchOutput: (patch: Partial<OutputCfg>) => void;
  /** Like `patchOutput({ background_color })`, but the running engine hears of it at most every ~100 ms (colour pickers). */
  setBackground: (abgr: number) => void;
  toggleRun: () => Promise<void>;
  togglePause: () => void;
  setAutostart: (on: boolean) => Promise<void>;
  setCloseToTray: (on: boolean) => void;
  setPreviewOn: (on: boolean) => void;
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
    const { source, output, controls, chain, closeToTray, autostartDefaulted, nvidiaPromptDismissed, activePreset, presetBaseline, wasRunning, previewOn } = get();
    // Only the folded cards that still exist; entries of removed filters and "expanded" flags are not worth keeping.
    const folded = get().collapsed;
    const collapsed = Object.fromEntries(chain.filter((f) => folded[f.id]).map((f) => [f.id, true]));
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ source, output, controls, chain, collapsed, closeToTray, autostartDefaulted, nvidiaPromptDismissed, activePreset, presetBaseline, wasRunning, previewOn }),
    );
  };

  const fail = (e: unknown) => set({ error: String(e) });

  // ---- live chain: every change goes to the backend (`set_chain` is cheap, also while the engine is stopped). A drag
  // makes many changes a frame; they are folded into one call per animation frame, one call in flight at a time, and the
  // last call always carries the newest chain.
  let chainDirty = false;
  let chainQueued = false;
  let chainInflight = false;
  const sendChain = () => {
    if (chainInflight || !chainDirty) return;
    chainDirty = false;
    chainInflight = true;
    api
      .setChain(get().chain)
      .catch(fail)
      .finally(() => {
        chainInflight = false;
        if (chainDirty) pushChain();
      });
  };
  const pushChain = () => {
    chainDirty = true;
    if (chainQueued) return;
    chainQueued = true;
    // A hidden window does not run animation frames; the timer makes sure the call still goes out.
    const go = () => {
      if (!chainQueued) return;
      chainQueued = false;
      sendChain();
    };
    requestAnimationFrame(go);
    setTimeout(go, 50);
  };

  const setChain = (chain: FilterCfg[]) => {
    const before = get().chain;
    const errors = get().filterErrors;
    // A failure message is about one version of one entry: it goes when the entry changes or is gone. Two exceptions:
    // "could not load <file>" stays while the file's path is the same (a slider moved; the engine tells again only
    // when the path or the file changes, or when the load works), and the message of an NVIDIA filter stays until the
    // engine clears it (it re-checks when the mode or picture size changes, and says so).
    const kept = Object.fromEntries(
      Object.entries(errors).filter(([id, message]) => {
        const was = before.find((f) => f.id === id);
        const now = chain.find((f) => f.id === id);
        if (was === undefined || now === undefined) return false;
        if (was === now) return true;
        return (
          was.kind === now.kind &&
          (FILTERS[now.kind].rtx === true ||
            (message.startsWith("could not load ") &&
              FILTERS[now.kind].params.every((p) => p.type !== "path" || was.params[p.key] === now.params[p.key])))
        );
      }),
    );
    // A status line belongs to the card: it goes with it.
    const statuses = get().filterStatus;
    const keptStatus = Object.fromEntries(Object.entries(statuses).filter(([id]) => chain.some((f) => f.id === id)));
    set({
      chain,
      ...(Object.keys(kept).length !== Object.keys(errors).length && { filterErrors: kept }),
      ...(Object.keys(keptStatus).length !== Object.keys(statuses).length && { filterStatus: keptStatus }),
    });
    persist();
    pushChain();
  };

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
  /** The camera the saved source names, among the connected ones. */
  const selectedCam = () => {
    const { cameras, source } = get();
    return cameras ? findCamera(cameras, source.video_device_id) : null;
  };
  const record = () => controlsRecord(get().controls, selectedCam());
  const pushSource = () => {
    if (running()) api.setSource(get().source, record()).catch(fail);
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
      await api.startEngine(source, output, chain, record());
      // The events may already have said more than "standby"; only fill in when they have not.
      if (!running()) set({ engine: { state: "standby", reason: "opening" } });
      set({ error: null, paused: false, wasRunning: true, filterErrors: {} });
      persist();
      return true;
    } catch (e) {
      fail(e);
      return false;
    }
  };

  // ---- camera controls: each control has a small queue so a drag sends at most ~20 writes a second, never piles up
  // behind a slow one (the first write to white balance, focus or exposure takes up to 700 ms), and always ends with
  // the value the slider stopped at.
  const CONTROL_GAP_MS = 50;
  interface Step {
    id: string;
    value: number;
    auto: boolean;
  }
  interface Pump {
    pending: Step[];
    busy: Promise<void> | null;
    last: number;
    timer?: ReturnType<typeof setTimeout>;
  }
  const pumps = new Map<ControlKey, Pump>();
  let controlsSeq = 0;

  const pumpOf = (key: ControlKey): Pump => {
    let p = pumps.get(key);
    if (!p) pumps.set(key, (p = { pending: [], busy: null, last: 0 }));
    return p;
  };
  const queued = (key: ControlKey) => {
    const p = pumps.get(key);
    return !!p && (p.busy !== null || p.pending.length > 0);
  };
  const putControl = (info: ControlInfo) =>
    set((s) => (s.controlInfos ? { controlInfos: s.controlInfos.map((i) => (i.key === info.key ? info : i)) } : {}));

  /** Sends the next queued step of `key` when the previous one is done and the gap has passed. */
  const drain = (key: ControlKey) => {
    const p = pumpOf(key);
    if (p.busy || p.pending.length === 0 || p.timer !== undefined) return;
    const wait = CONTROL_GAP_MS - (performance.now() - p.last);
    if (wait > 0) {
      p.timer = setTimeout(() => {
        p.timer = undefined;
        drain(key);
      }, wait);
      return;
    }
    const step = p.pending.shift()!;
    p.last = performance.now();
    p.busy = api
      .setCameraControl(step.id, key, step.value, step.auto)
      .then((info) => {
        if (p.pending.length === 0 && get().controlsFor === step.id) putControl(info);
      })
      .catch((e) => {
        p.pending = [];
        say(`${key.replace(/_/g, " ")}: ${String(e)}`);
        void get().refreshControls();
      })
      .finally(() => {
        p.busy = null;
        drain(key);
      });
  };

  /** Queues steps for `key`; steps that were not sent yet are replaced (the newest slider position wins). */
  const queueControl = (key: ControlKey, id: string, steps: Omit<Step, "id">[]) => {
    pumpOf(key).pending = steps.map((s) => ({ ...s, id }));
    drain(key);
  };

  /** Shows `info` for its control now and stores it when it differs from the device's own state. */
  const showControl = (cam: { id: string; name: string }, info: ControlInfo) => {
    putControl(info);
    set({ controls: rememberControl(get().controls, cam, info) });
    persist();
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
    filterErrors: {},
    filterStatus: {},
    presets: [],
    controlInfos: null,
    controlsFor: null,
    controlsError: null,
    controlsResetting: false,
    vcam: null,
    vcamBusy: null,
    vcamEpoch: 0,
    nvidia: null,
    gpu: null,
    gpuRead: false,
    nvidiaPrompt: false,
    nvidiaInstall: null,
    nvidiaError: null,

    refreshNvidia: async () => {
      try {
        const nvidia = await api.nvidiaStatus();
        // Both at once, so a card never sees "no GPU" for the moment between the two answers.
        const gpu = get().gpu ?? (await api.nvidiaGpu());
        set({ nvidia, gpu, gpuRead: true });
      } catch {
        /* the dialog and the cards keep what they showed; the next focus asks again */
      }
    },

    openNvidiaPrompt: () => set({ nvidiaPrompt: true, nvidiaError: null }),

    closeNvidiaPrompt: () => {
      set({ nvidiaPrompt: false, nvidiaPromptDismissed: true });
      persist();
    },

    installNvidia: async () => {
      const { gpu, nvidiaInstall } = get();
      if (!gpu || nvidiaInstall) return;
      set({ nvidiaInstall: { stage: "download", done: 0, total: 0 }, nvidiaError: null });
      try {
        await api.installNvidiaSdk(gpu.arch);
        const nvidia = await api.nvidiaStatus();
        set({
          nvidia,
          nvidiaPrompt: !nvidia.available,
          nvidiaError: nvidia.available ? null : `The installer closed, but the runtime is not ready: ${nvidia.detail}`,
        });
        if (nvidia.available) say("NVIDIA video effects installed");
      } catch (e) {
        set({ nvidiaError: String(e) });
      } finally {
        set({ nvidiaInstall: null });
      }
    },

    refreshVcam: async () => {
      if (get().vcamBusy) return;
      try {
        const vcam = await api.vcamStatus();
        if (get().vcamBusy) return;
        const changed = get().vcam?.state !== vcam.state;
        set((s) => ({ vcam, ...(changed && { vcamEpoch: s.vcamEpoch + 1 }) }));
      } catch {
        /* the banner keeps what it showed; the next focus asks again */
      }
    },

    runVcam: async (action) => {
      if (get().vcamBusy) return;
      // The backend lets go of the frame file before it asks for administrator rights (the helper may have to replace
      // it), so a running engine stops; it starts again afterwards. The prompt may stay open as long as it likes: the
      // buttons stay disabled until the helper has exited, and nothing else in the window waits for it.
      const resume = running();
      set({ vcamBusy: action, ...(resume && { engine: STOPPED, paused: false }) });
      if (resume) useStats.setState({ stats: null });
      try {
        const result = await api.vcamInstall();
        // A Start that was refused for want of the camera left its error up; it is no longer true.
        if (get().error?.includes("virtual camera is not installed")) set({ error: null });
        const what = action === "repair" ? "repaired" : action === "update" ? "updated" : "installed";
        // The frame file belongs to one Windows account at a time: this one, from now on (a repair by another
        // account takes it over; that account repairs it again to get it back).
        say(
          result === "pending_restart"
            ? `Virtual camera ${what} for this Windows account. It takes effect after the camera service restarts (close the apps that use a camera) or after Windows restarts.`
            : `Virtual camera ${what}. It now works for this Windows account.`,
        );
      } catch (e) {
        say(String(e));
      } finally {
        set((s) => ({ vcamBusy: null, vcamEpoch: s.vcamEpoch + 1 }));
        await get().refreshVcam();
        if (resume && get().vcam?.state !== "missing") await begin();
      }
    },

    removeVcam: async () => {
      if (get().vcamBusy) return;
      // The backend shuts the engine down before it asks for administrator rights; the window follows at once.
      set({ vcamBusy: "remove", engine: STOPPED, paused: false, wasRunning: false });
      useStats.setState({ stats: null });
      persist();
      try {
        const result = await api.vcamUninstall();
        say(
          result === "pending_restart"
            ? "Virtual camera removed. A file it used is deleted when Windows restarts."
            : "Virtual camera removed",
        );
      } catch (e) {
        say(String(e));
      } finally {
        set((s) => ({ vcamBusy: null, vcamEpoch: s.vcamEpoch + 1 }));
        await get().refreshVcam();
      }
    },

    init: async () => {
      await api.onEngineState((e) => {
        set({ engine: { state: e.state, reason: e.reason ?? null }, ...(e.state === "stopped" && { paused: false, filterStatus: {} }) });
        if (e.state === "stopped") useStats.setState({ stats: null });
      });
      await api.onEngineError((error) => {
        api.stopEngine().catch(() => {});
        set({ error, engine: STOPPED, paused: false, wasRunning: false });
        useStats.setState({ stats: null });
        persist();
      });
      await api.onFilterError(({ id, message }) => {
        // An empty message clears the card's line (the file loaded, or its path was cleared).
        if (message === "") {
          if (id in get().filterErrors) {
            set((s) => {
              const { [id]: _gone, ...rest } = s.filterErrors;
              return { filterErrors: rest };
            });
          }
          return;
        }
        // Only for a card that exists; the card shows it until its entry changes.
        if (get().chain.some((f) => f.id === id)) set((s) => ({ filterErrors: { ...s.filterErrors, [id]: message } }));
      });
      await api.onFilterStatus(({ id, message }) => {
        // An empty message clears the line. Only a card that exists shows one.
        if (message === "") {
          if (id in get().filterStatus) {
            set((s) => {
              const { [id]: _gone, ...rest } = s.filterStatus;
              return { filterStatus: rest };
            });
          }
          return;
        }
        if (get().chain.some((f) => f.id === id)) set((s) => ({ filterStatus: { ...s.filterStatus, [id]: message } }));
      });
      await api.onStats((stats) => useStats.setState({ stats }));
      await api.onNvidiaInstall((p) => get().nvidiaInstall && set({ nvidiaInstall: p }));
      await api.onDevicesChanged(() => void get().refreshCameras());
      await api.onControlsChanged((infos) => {
        // The values the device reports now: a control that has writes waiting keeps what the user set.
        const cur = get().controlInfos;
        const next = sortControls(infos).map((i) => (queued(i.key) ? (cur?.find((c) => c.key === i.key) ?? i) : i));
        if (JSON.stringify(next) !== JSON.stringify(cur)) set({ controlInfos: next, controlsFor: selectedCam()?.id ?? get().controlsFor });
      });
      // The virtual camera is read at startup, after each action (above) and whenever the window gets focus back
      // (a repair by hand, or `vcam-tool`, shows up without a restart).
      const vcamRead = get().refreshVcam();
      window.addEventListener("focus", () => void get().refreshVcam());
      // NVIDIA's runtime is read at startup (the install dialog opens from this, once: "Not now" is remembered), after an
      // install and whenever the window gets focus back, so installing it by hand shows up without a restart.
      void get()
        .refreshNvidia()
        .then(() => {
          const { gpu, nvidia, nvidiaPromptDismissed } = get();
          if (gpu && nvidia && !nvidia.available && !nvidiaPromptDismissed) set({ nvidiaPrompt: true });
        });
      window.addEventListener("focus", () => void get().refreshNvidia());
      await Promise.all([get().refreshCameras(), get().refreshPresets()]).catch(fail);

      // Work out which saved preset (if any) the restored chain still matches, so the toolbar shows the right name.
      const { presets } = get();
      if (get().activePreset && !presets.includes(get().activePreset!)) set({ activePreset: null, presetBaseline: null });
      if (!get().activePreset) {
        const sig = signature(get().chain);
        for (const name of presets) {
          const saved = await api.loadPreset(name).catch(() => null);
          if (saved && signature(saved) === sig) {
            set({ activePreset: name, presetBaseline: sig });
            break;
          }
        }
      }
      persist();

      try {
        // Release builds start with Windows by default. A dev build is not installed anywhere, and its Run entry would
        // launch `target\debug\better-cam.exe` at login without the dev server: it stays off unless set by hand.
        if (!get().autostartDefaulted && !IS_DEV_BUILD) {
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
      // Without the virtual camera the start would only be refused; the banner already says what to do.
      await vcamRead;
      if (get().wasRunning && get().vcam?.state !== "missing") {
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

    refreshControls: async () => {
      const cam = selectedCam();
      const seq = ++controlsSeq;
      if (!cam) {
        set({ controlInfos: null, controlsFor: null, controlsError: null });
        return;
      }
      if (get().controlsFor !== cam.id) set({ controlInfos: null, controlsFor: cam.id, controlsError: null });
      try {
        const infos = await api.getCameraControls(cam.id);
        if (seq === controlsSeq) set({ controlInfos: sortControls(infos), controlsError: null });
      } catch (e) {
        if (seq === controlsSeq) set({ controlInfos: [], controlsError: String(e) });
      }
    },

    setControl: (key, patch) => {
      const cam = selectedCam();
      const info = get().controlInfos?.find((i) => i.key === key);
      if (!cam || !info || get().controlsFor !== cam.id) return;
      const value = patch.value === undefined ? info.value : Math.min(info.max, Math.max(info.min, Math.round(patch.value)));
      const auto = patch.auto ?? (patch.value === undefined ? info.auto : false);
      if (value === info.value && auto === info.auto) return;
      showControl(cam, { ...info, value, auto });
      queueControl(key, cam.id, [{ value, auto }]);
    },

    resetControl: (key) => {
      const cam = selectedCam();
      const info = get().controlInfos?.find((i) => i.key === key);
      if (!cam || !info || get().controlsFor !== cam.id) return;
      showControl(cam, { ...info, value: info.def, auto: info.can_auto });
      // The device's default value first, so the manual value behind an Auto control is the default too, then Auto.
      queueControl(key, cam.id, [{ value: info.def, auto: false }, ...(info.can_auto ? [{ value: info.def, auto: true }] : [])]);
    },

    resetAllControls: async () => {
      const cam = selectedCam();
      if (!cam || get().controlsResetting) return;
      set({ controlsResetting: true });
      try {
        for (const p of pumps.values()) {
          p.pending = [];
          clearTimeout(p.timer);
          p.timer = undefined;
        }
        await Promise.all([...pumps.values()].map((p) => p.busy));
        set({ controls: forgetControls(get().controls, cam) });
        persist();
        const infos = await api.resetCameraControls(cam.id);
        if (get().controlsFor === cam.id) set({ controlInfos: sortControls(infos), controlsError: null });
        say("Camera controls reset to the device defaults");
      } catch (e) {
        say(`Reset failed: ${String(e)}`);
        void get().refreshControls();
      } finally {
        set({ controlsResetting: false });
      }
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

    setPreviewOn: (previewOn) => {
      set({ previewOn });
      persist();
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

    toggleCollapsed: (id) => {
      set((s) => ({ collapsed: { ...s.collapsed, [id]: !s.collapsed[id] } }));
      persist();
    },
    setAllCollapsed: (on) => {
      set((s) => ({ collapsed: Object.fromEntries(s.chain.map((f) => [f.id, on])) }));
      persist();
    },

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
        const chain = await api.loadPreset(name);
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
