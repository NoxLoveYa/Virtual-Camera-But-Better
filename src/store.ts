import { create } from "zustand";
import { api, type EngineState, type NvidiaGpu, type NvidiaInstallProgress, type NvidiaStatus, type Stats, type VcamStatus } from "./api";
import { defOf, defaultChain, labelOf, makeFilter, type ExtensionInfo, type FilterCfg, type FilterKind, type ParamValue } from "./filters/schema";
import { normalizeChain, presetChain, refreshExtParams, signature } from "./filters/chain";
import { presetNameFor, stemOf } from "./ui/fileDrop";
import { DEFAULT_UI, normalizeUi, type UiState } from "./ui/layout";
import { pointFilter, samePoint, type PreviewPoint } from "./ui/previewFrame";
import { createSaver } from "./ui/saver";
import { createReadGate, wantsVcamPoll } from "./ui/vcamPoll";
import {
  DEFAULT_OUTPUT,
  DEFAULT_SOURCE,
  autoCfgOf,
  modeOf,
  normalizeAutoCfg,
  normalizeControls,
  normalizeOutput,
  normalizeSource,
  repairSource,
  controlsRecord,
  touchesCapture,
  effectiveSource,
  findCamera,
  findStored,
  forgetControls,
  isHandBackState,
  withAutoCfg,
  withLearned,
  withResume,
  afterVcamRemoval,
  inUseAfterStart,
  inUseAfterState,
  inUseDeviceDefault,
  inUseFromFallback,
  pausedAfterState,
  rememberControl,
  savedAfterEdit,
  shouldPushSettings,
  sortControls,
  statusOf,
  type AutoCfg,
  type AutoLoop,
  type AutoStatus,
  type CameraInfo,
  type ControlInfo,
  type ControlKey,
  type ControlMode,
  type InUse,
  type OutputCfg,
  type SourceCfg,
  type StoredControls,
} from "./camera/schema";

const STORAGE_KEY = "better-cam";
/** The most often `localStorage` is written (see `persist`). */
const SAVE_GAP_MS = 300;
/** How often the virtual camera's status is read again while a problem with its data folder is shown. */
const VCAM_PROBLEM_POLL_MS = 10_000;
/** The timer of that poll (one for the page, whatever number of times `init` runs). */
let vcamTimer: number | undefined;
/** The status reads that are pending or done, for the poll and the focus read (one for the page). */
const vcamReads = createReadGate();

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
  /** Set once the NVIDIA install prompt was closed, so adding another NVIDIA filter does not ask again. */
  nvidiaPromptDismissed: boolean;
  /** The preset the chain was last loaded from or saved as, and what it looked like then (see `signature`). */
  activePreset: string | null;
  presetBaseline: string | null;
  /** Whether the engine was running when the app last ran, so the next launch starts it again. */
  wasRunning: boolean;
  /** The live preview in the window; off saves the IPC and the CPU it costs. */
  previewOn: boolean;
  /** The column split and which panels are open (Camera, Camera controls, Output, Advanced). */
  ui: UiState;
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
  /** What kind of problem `reason` is (see `EngineState.kind`): the banner picks its text and button by it. */
  kind: string | null;
  /** The programs that hold the camera while `kind` is `in_use` (see `EngineState.apps`); empty when none is known. */
  apps: string[];
  /** `apps` may include a program that only reads the virtual camera. */
  appsMaybe: boolean;
}

const STOPPED: EngineInfo = { state: "stopped", reason: null, kind: null, apps: [], appsMaybe: false };
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
    ui: DEFAULT_UI,
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
        ui: normalizeUi(s.ui),
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
  /** Pause is not an engine state: the backend reports it (`engine-paused`) and this follows; a stop clears it. */
  paused: boolean;
  /**
   * What runs instead of the saved format when the camera could not deliver it (`format-fallback`, or "Use device
   * default" after a format error). Not saved: the saved settings stay the user's choice, the next Start tries them
   * again, and the panel shows this and the saved format.
   */
  inUse: InUse | null;
  error: string | null;
  /** The `kind` the engine gave the stop that `error` is about (`format`: the camera lacks the format asked for). */
  errorKind: string | null;
  notice: string | null;
  /** Bumped on every toast so repeating the same message restarts its timer. */
  noticeSeq: number;
  undo: Snapshot | null;
  /** What the engine said when a filter failed, by filter id; gone when that filter's entry changes. */
  filterErrors: Record<string, string>;
  /** Lines the engine shows on a card that are not errors ("Loading model…"), by filter id; gone when the engine stops or the card is. */
  filterStatus: Record<string, string>;
  /** The picture the preview shows (see `setPreviewPoint`); `null` is the output. Not saved. */
  previewPoint: PreviewPoint;
  presets: string[];
  /** The extensions the app found (Settings → Extensions); empty until `extensionsRead`. Not saved: the folder is the truth. */
  extensions: ExtensionInfo[];
  /** `extensions` has been read once, so a card of an extension that is not in it is "not installed" and not "not read yet". */
  extensionsRead: boolean;
  /** The Extensions dialog (Settings → Extensions) is open. */
  extensionsOpen: boolean;
  /** What the selected camera's controls say now; `null` until they were read. */
  controlInfos: ControlInfo[] | null;
  /** The camera `controlInfos` belong to. */
  controlsFor: string | null;
  controlsError: string | null;
  /** Reset all is running. */
  controlsResetting: boolean;
  /**
   * What Better Cam auto does for the camera that runs (`auto-status`; its `learned` and `resume` are persisted when
   * they arrive and not kept here). `null` until the first one, and after the engine stops.
   */
  autoStatus: AutoStatus | null;
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
  /**
   * Who sets a control: Manual, Camera auto or Better Cam auto (`app`, exposure and white balance). The exposure
   * carries the gain with it. Stored per camera; works while the camera is closed (the loop starts at the next open).
   */
  setControlMode: (key: ControlKey, mode: ControlMode) => Promise<void>;
  /** Changes the camera's Better Cam auto settings (target, longest exposure, most gain, warmth, response); live, stored per camera. */
  setAutoConfig: (patch: { exposure?: Partial<AutoCfg["exposure"]>; white_balance?: Partial<AutoCfg["white_balance"]> }) => void;
  /** The status line's button: starts a paused loop again (`conflict`) or, after "ignored", clears the block and tries Better Cam auto again. */
  resumeAuto: (key: AutoLoop) => Promise<void>;
  /** `auto-status`: shows it, stores its `learned` and `resume`, and says why when Better Cam auto handed a control back. */
  applyAutoStatus: (status: AutoStatus) => void;
  /** Changes camera settings (repaired against the device), persists them and, while running, sends them at once. */
  patchSource: (patch: Partial<SourceCfg>) => void;
  patchOutput: (patch: Partial<OutputCfg>) => void;
  /** Like `patchOutput({ background_color })`, but the running engine hears of it at most every ~100 ms (colour pickers). */
  setBackground: (abgr: number) => void;
  toggleRun: () => Promise<void>;
  /** Starts with the device default of the camera, as the format that runs, and keeps the saved settings (the banner's button). */
  startWithDeviceDefault: () => Promise<void>;
  /** Asks the running engine for the saved format again (the panel's button). */
  trySavedFormat: () => void;
  togglePause: () => void;
  setAutostart: (on: boolean) => Promise<void>;
  setCloseToTray: (on: boolean) => void;
  setPreviewOn: (on: boolean) => void;
  /**
   * Which picture the preview shows (L1): `null` the output (what apps get), `"input"` the camera before the chain, or the picture
   * entering a filter. For the length of a gesture only (hold to compare, the eyedropper, the crop handles): whoever sets it puts it
   * back to `null`. It also goes back by itself when the filter leaves the chain, the preview is turned off, the engine stops or
   * the window is hidden. The output to apps never changes. Not saved.
   */
  setPreviewPoint: (point: PreviewPoint) => void;
  /** Remembers the column split or a panel's fold (see `Saved.ui`). */
  setUi: (patch: Partial<UiState>) => void;
  dismissError: () => void;
  dismissNotice: () => void;
  undoChain: () => void;

  /** A toast without an Undo (a dropped file Better Cam cannot use, a preset that failed to import). */
  notify: (text: string) => void;
  /** Adds a sentence to the toast on screen, or shows it alone when there is none (the note on a drop of several files). */
  addNotice: (text: string) => void;
  /** Adds a filter at gap `at` of the chain (default: the end) with some of its params set (a dropped file's path). */
  addFilter: (kind: FilterKind, at?: number | null, params?: Record<string, ParamValue>) => void;
  removeFilter: (id: string) => void;
  moveFilter: (from: number, to: number) => void;
  toggleFilter: (id: string) => void;
  setParam: (id: string, key: string, value: ParamValue) => void;
  toggleCollapsed: (id: string) => void;
  setAllCollapsed: (on: boolean) => void;

  /** Takes a new extension list: coerces the `ext:` cards to the manifests (a reloaded extension may have other params). */
  applyExtensions: (list: ExtensionInfo[]) => void;
  /** Reads the folder again now (Settings → Extensions → Reload). */
  reloadExtensions: () => Promise<void>;
  setExtensionEnabled: (id: string, on: boolean) => Promise<void>;
  openExtensionsFolder: () => Promise<void>;
  setExtensionsOpen: (open: boolean) => void;

  refreshPresets: () => Promise<void>;
  /** Saves the current chain under `name` (replacing any preset with that name) and makes it the active preset. */
  savePreset: (name: string) => Promise<void>;
  saveActivePreset: () => Promise<void>;
  loadPreset: (name: string) => Promise<void>;
  /**
   * Imports a preset file from anywhere (a drop): saves it as a preset under its file name ("Name 2" on a clash) and
   * loads it, with Undo for the chain. The file stays. A failure is a toast.
   */
  importPreset: (path: string) => Promise<void>;
  deletePreset: (name: string) => Promise<void>;
  resetChain: () => void;
}

export const useStore = create<State>((set, get) => {
  const write = () => {
    const { source, output, controls, chain, closeToTray, autostartDefaulted, nvidiaPromptDismissed, activePreset, presetBaseline, wasRunning, previewOn, ui } = get();
    // Only the folded cards that still exist; entries of removed filters and "expanded" flags are not worth keeping.
    const folded = get().collapsed;
    const collapsed = Object.fromEntries(chain.filter((f) => folded[f.id]).map((f) => [f.id, true]));
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ source, output, controls, chain, collapsed, closeToTray, autostartDefaulted, nvidiaPromptDismissed, activePreset, presetBaseline, wasRunning, previewOn, ui }),
    );
  };
  // A slider drag changes the state many times a second and each save serialises everything: saves are folded into one
  // write at most every SAVE_GAP_MS (it reads the state when it runs, so the stored value is the newest one).
  const saver = createSaver(write, SAVE_GAP_MS);
  /** For changes that come in bursts (sliders, colour pickers, the splitter, folding). */
  const persist = saver.schedule;
  /** For the rare ones that other code or the next launch relies on at once (the engine's run state, a saved preset). */
  const persistNow = saver.now;
  // The window going away, hidden or reloaded must not lose the save that is waiting. A quit from the tray menu or a
  // hide to the tray fires none of these page events, so the app asks the page to save before it exits or hides
  // (`flush-save`, answered when done; see `RunEvent::ExitRequested` in main.rs).
  if (typeof window !== "undefined") {
    window.addEventListener("beforeunload", saver.flush);
    window.addEventListener("pagehide", saver.flush);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState !== "hidden") return;
      saver.flush();
      // A hidden window shows no picture (the backend puts the output back as well).
      endPreviewPoint();
    });
    // Outside the app (plain browser, no Tauri) there is nobody to ask: the page events above are all there is.
    api.onFlushSave(saver.flush).catch(() => {});
  }

  const fail = (e: unknown) => set({ error: String(e) });

  /** A gesture that showed another picture than the output is over: the preview is the output again. */
  const endPreviewPoint = () => {
    if (get().previewPoint !== null) get().setPreviewPoint(null);
  };

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

  /** `burst`: one of many changes a second (a slider drag), so the save may wait; any other edit is saved at once. */
  const setChain = (chain: FilterCfg[], burst = false) => {
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
        const def = defOf(now.kind, get().extensions);
        return (
          was.kind === now.kind &&
          (def?.rtx === true ||
            (message.startsWith("could not load ") &&
              (def?.params ?? []).every((p) => p.type !== "path" || was.params[p.key] === now.params[p.key])))
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
    // The preview point names a card: it ends with the card (the engine puts the output back by itself as well).
    const pointId = pointFilter(get().previewPoint);
    if (pointId !== null && !chain.some((f) => f.id === pointId)) endPreviewPoint();
    if (burst) persist();
    else persistNow();
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
  /** Edits reach the backend while it runs and while the virtual camera is changed (it keeps them for the new engine). */
  const pushing = () => shouldPushSettings(get().engine.state, get().vcamBusy);
  /** The camera the saved source names, among the connected ones. */
  const selectedCam = () => {
    const { cameras, source } = get();
    return cameras ? findCamera(cameras, source.video_device_id) : null;
  };
  const record = () => controlsRecord(get().controls, selectedCam());
  /** What the engine is sent: the saved source, with the format that runs while a fallback does. */
  const runningSource = () => effectiveSource(get().source, get().inUse);
  const pushSource = () => {
    if (pushing()) api.setSource(runningSource(), record()).catch(fail);
  };
  const pushOutput = () => {
    if (pushing()) api.setOutput(get().output).catch(fail);
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

  /**
   * Starts the engine with the saved state; false when the call was rejected (the error banner says why). A Start
   * tries the saved format, so an earlier fallback is forgotten; `deviceDefault` starts with the device default as the
   * format that runs (the saved settings stay as they are).
   */
  const begin = async (deviceDefault = false): Promise<boolean> => {
    const { source, output, chain } = get();
    const inUse = deviceDefault ? inUseDeviceDefault(source) : null;
    set({ inUse });
    try {
      await api.startEngine(effectiveSource(source, inUse), output, chain, record());
      // The events may already have said more than "standby"; only fill in when they have not. Not while the virtual
      // camera is being changed: the engine is released then (the events said so) and starts when the change ends.
      if (!running() && !get().vcamBusy) set({ engine: { ...STOPPED, state: "standby", reason: "opening" } });
      set({ error: null, errorKind: null, paused: false, wasRunning: true, filterErrors: {} });
      persistNow();
      return true;
    } catch (e) {
      // A rejected Start runs nothing: no "using the device default" panel for it.
      set({ inUse: inUseAfterStart(inUse, false) });
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

  /** The control keys a loop sets: the exposure loop sets the gain too. */
  const keysOf = (loop: AutoLoop): ControlKey[] => (loop === "exposure" ? ["exposure", "gain"] : ["white_balance"]);

  /** Drops the slider writes still waiting for `keys` (a mode change ends them) and waits for the one in flight. */
  const settleWrites = async (keys: ControlKey[]) => {
    for (const k of keys) {
      const p = pumps.get(k);
      if (!p) continue;
      p.pending = [];
      clearTimeout(p.timer);
      p.timer = undefined;
    }
    await Promise.all(keys.map((k) => pumps.get(k)?.busy));
  };

  /**
   * After Better Cam auto handed a loop back (the camera went to Camera auto, or to its earlier value): reads the
   * controls again and stores them as they are now, so the record no longer says Better Cam auto.
   */
  const followHandBack = async (cam: { id: string; name: string }, loop: AutoLoop) => {
    try {
      const infos = await api.getCameraControls(cam.id);
      if (get().controlsFor === cam.id) set({ controlInfos: sortControls(infos) });
      let controls = get().controls;
      for (const key of keysOf(loop)) {
        const i = infos.find((x) => x.key === key);
        if (i && modeOf(i) !== "app") controls = rememberControl(controls, cam, i);
      }
      set({ controls });
      persist();
    } catch {
      /* the next read of the controls shows the truth */
    }
  };

  let autoTimer: ReturnType<typeof setTimeout> | undefined;
  /** Sends the camera's Better Cam auto settings, at most about every 80 ms while a slider moves; the last call has the newest values. */
  const pushAutoConfig = () => {
    if (autoTimer !== undefined) return;
    autoTimer = setTimeout(() => {
      autoTimer = undefined;
      const cam = selectedCam();
      if (cam) api.setAutoConfig(cam.id, autoCfgOf(get().controls, cam)).catch((e) => say(`Better Cam auto: ${String(e)}`));
    }, 80);
  };

  const finish = async () => {
    try {
      await api.stopEngine();
      set({ engine: STOPPED, paused: false, inUse: null, wasRunning: false });
      useStats.setState({ stats: null });
      persistNow();
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
    inUse: null,
    error: null,
    errorKind: null,
    notice: null,
    noticeSeq: 0,
    undo: null,
    filterErrors: {},
    filterStatus: {},
    previewPoint: null,
    presets: [],
    extensions: [],
    extensionsRead: false,
    extensionsOpen: false,
    controlInfos: null,
    controlsFor: null,
    controlsError: null,
    controlsResetting: false,
    autoStatus: null,
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
      persistNow();
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
      // A focus read and a tick can overlap: an answer older than one already shown is dropped (see `createReadGate`).
      const read = vcamReads.begin();
      let vcam: VcamStatus | null = null;
      try {
        vcam = await api.vcamStatus();
      } catch {
        /* the banner keeps what it showed; the next focus asks again */
      }
      if (!vcamReads.end(read, vcam !== null) || vcam === null || get().vcamBusy) return;
      const changed = get().vcam?.state !== vcam.state;
      set((s) => ({ vcam, ...(changed && { vcamEpoch: s.vcamEpoch + 1 }) }));
    },

    runVcam: async (action) => {
      if (get().vcamBusy) return;
      // The backend lets go of the frame file before it asks for administrator rights (the helper may have to replace
      // it): the engine stops (an `engine-state` event says so, so the window shows what is true), and the backend
      // starts a new one with the state the old one had when the change ends, whether it worked or not. The prompt
      // may stay open as long as it likes: the buttons stay disabled until the helper has exited, and nothing else in
      // the window waits for it.
      set({ vcamBusy: action });
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
      }
    },

    removeVcam: async () => {
      if (get().vcamBusy) return;
      // The backend shuts the engine down before it asks for administrator rights; the window follows at once. It
      // brings the engine back with the same state when the removal is declined or fails, so the pause, the fallback
      // and `wasRunning` are cleared only once the removal happened.
      set({ vcamBusy: "remove", engine: STOPPED });
      useStats.setState({ stats: null });
      let removed = false;
      try {
        const result = await api.vcamUninstall();
        removed = true;
        say(
          result === "pending_restart"
            ? "Virtual camera removed. A file it used is deleted when Windows restarts."
            : "Virtual camera removed",
        );
      } catch (e) {
        say(String(e));
      } finally {
        set((s) => ({ vcamBusy: null, vcamEpoch: s.vcamEpoch + 1, ...afterVcamRemoval(removed, s) }));
        if (removed) persistNow();
        await get().refreshVcam();
      }
    },

    init: async () => {
      await api.onEngineState((e) => {
        set((s) => ({
          engine: {
            state: e.state,
            reason: e.reason ?? null,
            kind: e.kind ?? null,
            apps: e.apps ?? [],
            appsMaybe: e.apps_maybe ?? false,
          },
          // A stop clears the pause and the fallback, except the one the virtual camera change announces: the engine
          // comes back with both (the backend reports the pause again). The kind of a stop that has an error says what
          // the error is about (`format`).
          paused: pausedAfterState(s.paused, e),
          inUse: inUseAfterState(s.inUse, e),
          ...(e.state === "stopped" && { filterStatus: {}, autoStatus: null, ...(e.kind && { errorKind: e.kind }) }),
        }));
        if (e.state === "stopped") {
          useStats.setState({ stats: null });
          // No picture to look at: a gesture that was showing another one is over.
          endPreviewPoint();
        }
      });
      await api.onEnginePaused(({ paused }) => set({ paused }));
      await api.onEngineError((error) => {
        api.stopEngine().catch(() => {});
        // The `stopped` event with the error's kind follows this one.
        set({ error, errorKind: null, engine: STOPPED, paused: false, inUse: null, wasRunning: false });
        useStats.setState({ stats: null });
        persistNow();
      });
      await api.onFormatFallback((f) => {
        say(`${f.asked} isn't available; using device default (${f.used}).`);
        // What runs now is the device default at the size and rate the camera delivers. The saved settings stay the
        // user's choice (the next Start tries them again); the panel shows both. The engine holds these settings, so
        // later edits send them (anything else would reopen the camera).
        set({ inUse: inUseFromFallback(f) });
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
      // The extension list: the event first, so a change between the two calls is not lost. The list is complete from the
      // first call (the app reads the folder before the window loads), so cards of missing extensions are only then "not installed".
      await api.onExtensionsChanged((list) => get().applyExtensions(list));
      await api.listExtensions().then((list) => get().applyExtensions(list)).catch(() => set({ extensionsRead: true }));
      await api.onStats((stats) => useStats.setState({ stats }));
      await api.onNvidiaInstall((p) => get().nvidiaInstall && set({ nvidiaInstall: p }));
      await api.onDevicesChanged(() => void get().refreshCameras());
      await api.onControlsChanged((infos) => {
        // The values the device reports now: a control that has writes waiting keeps what the user set.
        const cur = get().controlInfos;
        const next = sortControls(infos).map((i) => (queued(i.key) ? (cur?.find((c) => c.key === i.key) ?? i) : i));
        if (JSON.stringify(next) !== JSON.stringify(cur)) set({ controlInfos: next, controlsFor: selectedCam()?.id ?? get().controlsFor });
      });
      await api.onAutoStatus((s) => get().applyAutoStatus(s));
      // The virtual camera is read at startup, after each action (above) and whenever the window gets focus back
      // (a repair by hand, or `vcam-tool`, shows up without a restart).
      const vcamRead = get().refreshVcam();
      window.addEventListener("focus", () => void get().refreshVcam());
      // While a problem is shown whose Repair button is hidden (an administrator has to run `cipher /d`), the status is
      // read again every 10 s, so the button comes back without a click on the window. The timer exists only then, and
      // a tick does nothing while the window is hidden (the focus read covers the way back) or a read is pending.
      const watchVcamProblem = () => {
        const want = wantsVcamPoll(get().vcam);
        if (want && vcamTimer === undefined) {
          vcamTimer = window.setInterval(
            () => document.visibilityState === "visible" && !vcamReads.busy() && void get().refreshVcam(),
            VCAM_PROBLEM_POLL_MS,
          );
        } else if (!want && vcamTimer !== undefined) {
          window.clearInterval(vcamTimer);
          vcamTimer = undefined;
        }
      };
      useStore.subscribe(watchVcamProblem);
      void vcamRead.then(watchVcamProblem);
      // NVIDIA's runtime is read at startup, after an install and whenever the window gets focus back, so installing it by
      // hand shows up without a restart. The install dialog does not open from here: `addFilter` opens it the first time
      // an NVIDIA filter is added, and a card's "Install it now" opens it later.
      void get().refreshNvidia();
      window.addEventListener("focus", () => void get().refreshNvidia());
      await Promise.all([get().refreshCameras(), get().refreshPresets()]).catch(fail);

      // Work out which saved preset (if any) the restored chain still matches, so the toolbar shows the right name.
      const { presets } = get();
      if (get().activePreset && !presets.includes(get().activePreset!)) set({ activePreset: null, presetBaseline: null });
      if (!get().activePreset) {
        const sig = signature(get().chain);
        for (const name of presets) {
          const saved = await api.loadPreset(name, get().extensions).catch(() => null);
          if (saved && signature(saved) === sig) {
            set({ activePreset: name, presetBaseline: sig });
            break;
          }
        }
      }
      persistNow();

      try {
        // Release builds start with Windows by default. A dev build is not installed anywhere, and its Run entry would
        // launch `target\debug\better-cam.exe` at login without the dev server: it stays off unless set by hand.
        if (!get().autostartDefaulted && !IS_DEV_BUILD) {
          await api.setAutostart(true);
          set({ autostartDefaulted: true });
          persistNow();
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
      // Better Cam auto sets the control (the gain with the exposure): the card shows it, the user does not change it.
      if (!cam || !info || get().controlsFor !== cam.id || modeOf(info) === "app") return;
      const value = patch.value === undefined ? info.value : Math.min(info.max, Math.max(info.min, Math.round(patch.value)));
      const auto = patch.auto ?? (patch.value === undefined ? info.auto : false);
      if (value === info.value && auto === info.auto) return;
      showControl(cam, { ...info, value, auto, mode: auto ? "camera" : "manual" });
      queueControl(key, cam.id, [{ value, auto }]);
    },

    resetControl: (key) => {
      const cam = selectedCam();
      const info = get().controlInfos?.find((i) => i.key === key);
      if (!cam || !info || get().controlsFor !== cam.id || modeOf(info) === "app") return;
      showControl(cam, { ...info, value: info.def, auto: info.can_auto, mode: info.can_auto ? "camera" : "manual" });
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

    setControlMode: async (key, mode) => {
      const cam = selectedCam();
      const info = get().controlInfos?.find((i) => i.key === key);
      if (!cam || !info || get().controlsFor !== cam.id || modeOf(info) === mode) return;
      const keys: ControlKey[] = key === "exposure" ? ["exposure", "gain"] : [key];
      // The card follows at once; the answer (the rows as the device reports them, after the first slow write) replaces it.
      set((s) => ({
        controlInfos: (s.controlInfos ?? []).map((i) => {
          if (i.key === key) return { ...i, mode, auto: mode === "camera" };
          if (key === "exposure" && i.key === "gain" && (mode === "app" || i.driven_by === "exposure")) {
            const { driven_by: _by, ...rest } = i;
            return mode === "app" ? { ...rest, mode: "app", auto: false, driven_by: "exposure" } : { ...rest, mode: "manual" };
          }
          return i;
        }),
      }));
      try {
        await settleWrites(keys);
        const changed = await api.setControlMode(cam.id, key, mode);
        if (get().controlsFor === cam.id) set((s) => ({ controlInfos: sortControls((s.controlInfos ?? []).map((i) => changed.find((c) => c.key === i.key) ?? i)) }));
        let controls = get().controls;
        for (const c of changed) controls = rememberControl(controls, cam, c);
        set({ controls });
        persist();
        pushSource();
      } catch (e) {
        say(`${key.replace(/_/g, " ")}: ${String(e)}`);
        void get().refreshControls();
      }
    },

    setAutoConfig: (patch) => {
      const cam = selectedCam();
      if (!cam) return;
      const cur = autoCfgOf(get().controls, cam);
      const next = normalizeAutoCfg({
        exposure: { ...cur.exposure, ...patch.exposure },
        white_balance: { ...cur.white_balance, ...patch.white_balance },
      })!;
      if (JSON.stringify(next) === JSON.stringify(cur)) return;
      set({ controls: withAutoCfg(get().controls, cam, next) });
      persist();
      pushAutoConfig();
    },

    resumeAuto: async (loop) => {
      const cam = selectedCam();
      if (!cam) return;
      const ignored = statusOf(get().autoStatus, cam, loop)?.state === "ignored";
      try {
        await api.resumeAuto(cam.id, loop);
        // "Try again" after the camera ignored Better Cam auto: the block is gone, so ask for the mode again.
        if (ignored) await get().setControlMode(loop, "app");
      } catch (e) {
        say(`${loop.replace(/_/g, " ")}: ${String(e)}`);
      }
    },

    applyAutoStatus: (status) => {
      const { learned, resume, ...shown } = status;
      const prev = get().autoStatus;
      const cams = get().cameras;
      const cam = (cams && findCamera(cams, status.id)) || { id: status.id, name: findStored(get().controls, { id: status.id, name: "" })?.device.name ?? "" };
      set({ autoStatus: shown });
      // What the loops learned and where they start next time belong to the camera's record.
      let controls = get().controls;
      if (learned) controls = withLearned(controls, cam, learned);
      if (resume) controls = withResume(controls, cam, resume);
      if (controls !== get().controls) {
        set({ controls });
        persist();
      }
      // A hand-back says why once (the line under the row says it as long as it holds), and the record follows it.
      for (const loop of ["exposure", "white_balance"] as const) {
        const now = shown[loop];
        const was = prev && prev.id === shown.id ? prev[loop] : undefined;
        if (!now?.handed_back || !isHandBackState(now.state) || (was?.state === now.state && was.handed_back)) continue;
        if (now.message) say(now.message);
        void followHandBack(cam, loop);
      }
    },

    patchSource: (patch) => {
      const prev = get().source;
      // An edit of the camera fields while a fallback runs starts from what the fields show, and is the new choice.
      const next = savedAfterEdit(prev, get().inUse, patch);
      const { cameras } = get();
      const fixed = cameras ? repairSource(next, cameras) : next;
      const picked = get().inUse !== null && touchesCapture(patch);
      if (JSON.stringify(fixed) === JSON.stringify(prev) && !picked) return;
      set({ source: fixed, ...(picked && { inUse: null }) });
      persist();
      pushSource();
    },

    trySavedFormat: () => {
      if (get().inUse === null) return;
      set({ inUse: null });
      pushSource();
    },

    startWithDeviceDefault: async () => {
      if (!running()) await begin(true);
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
      persistNow();
      api.setCloseToTray(closeToTray).catch(fail);
    },

    setPreviewOn: (previewOn) => {
      set({ previewOn });
      persist();
      if (!previewOn) endPreviewPoint();
    },
    setPreviewPoint: (previewPoint) => {
      if (samePoint(get().previewPoint, previewPoint)) return;
      set({ previewPoint });
      // The calls reach the backend in order; the engine falls back to the output by itself when it cannot show the point.
      api.setPreviewPoint(previewPoint).catch(() => {});
    },
    setUi: (patch) => {
      set((s) => ({ ui: normalizeUi({ ...s.ui, ...patch }) }));
      persist();
    },

    dismissError: () => set({ error: null, errorKind: null, notice: null, undo: null }),
    dismissNotice: () => set({ notice: null, undo: null }),
    undoChain: () => {
      const u = get().undo;
      if (!u) return;
      replaceChain(u.chain, u.activePreset, u.presetBaseline);
      say("Undone");
    },

    notify: (text) => say(text),
    addNotice: (text) =>
      set((s) => (s.notice ? { notice: `${s.notice} ${text}`, noticeSeq: s.noticeSeq + 1 } : { notice: text, undo: null, noticeSeq: s.noticeSeq + 1 })),

    addFilter: (kind, at, params) => {
      const prev = snapshot();
      const { extensions } = get();
      const made = makeFilter(kind, true, extensions);
      const filter = params ? { ...made, params: { ...made.params, ...params } } : made;
      const index = at == null ? prev.chain.length : Math.max(0, Math.min(prev.chain.length, at));
      setChain([...prev.chain.slice(0, index), filter, ...prev.chain.slice(index)]);
      say(`Added ${labelOf(kind, extensions)} as step ${index + 1}`, prev);
      // The first NVIDIA filter on an RTX machine without the runtime: offer the install now ("Not now" is remembered; the
      // card keeps its "Install it now"). A preset or a file drop that carries one does not ask.
      const { gpu, nvidia, nvidiaPromptDismissed } = get();
      if (defOf(kind, extensions)?.rtx && gpu && nvidia && !nvidia.available && !nvidiaPromptDismissed) get().openNvidiaPrompt();
    },
    removeFilter: (id) => {
      const prev = snapshot();
      const gone = prev.chain.find((f) => f.id === id);
      setChain(prev.chain.filter((f) => f.id !== id));
      if (gone) say(`Removed ${labelOf(gone.kind, get().extensions)}`, prev);
    },
    moveFilter: (from, to) => {
      const prev = snapshot();
      const moved = prev.chain[from];
      if (!moved || from === to || to < 0 || to >= prev.chain.length) return;
      const chain = [...prev.chain];
      chain.splice(to, 0, chain.splice(from, 1)[0]);
      setChain(chain);
      say(`Moved ${labelOf(moved.kind, get().extensions)} to step ${to + 1}`, prev);
    },
    toggleFilter: (id) => setChain(get().chain.map((f) => (f.id === id ? { ...f, enabled: !f.enabled } : f))),
    setParam: (id, key, value) =>
      setChain(get().chain.map((f) => (f.id === id ? { ...f, params: { ...f.params, [key]: value } } : f)), true),

    toggleCollapsed: (id) => {
      set((s) => ({ collapsed: { ...s.collapsed, [id]: !s.collapsed[id] } }));
      persist();
    },
    setAllCollapsed: (on) => {
      set((s) => ({ collapsed: Object.fromEntries(s.chain.map((f) => [f.id, on])) }));
      persist();
    },

    applyExtensions: (list) => {
      set({ extensions: list, extensionsRead: true });
      // The cards of an extension that was edited (or that just arrived) follow its manifest.
      const chain = refreshExtParams(get().chain, list);
      if (chain !== get().chain) setChain(chain);
    },
    reloadExtensions: async () => {
      try {
        get().applyExtensions(await api.reloadExtensions());
      } catch (e) {
        say(String(e));
      }
    },
    setExtensionEnabled: async (id, on) => {
      try {
        get().applyExtensions(await api.setExtensionEnabled(id, on));
      } catch (e) {
        say(String(e));
      }
    },
    openExtensionsFolder: async () => {
      try {
        await api.openExtensionsFolder();
      } catch (e) {
        say(String(e));
      }
    },
    setExtensionsOpen: (extensionsOpen) => set({ extensionsOpen }),

    refreshPresets: async () => set({ presets: await api.listPresets() }),
    savePreset: async (name) => {
      const { chain } = get();
      try {
        await api.savePreset(name, chain);
        set({ activePreset: name, presetBaseline: signature(chain) });
        persistNow();
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
        const chain = await api.loadPreset(name, get().extensions);
        const prev = snapshot();
        replaceChain(chain, name, signature(chain));
        say(`Loaded “${name}”`, signature(prev.chain) === signature(chain) ? null : prev);
      } catch (e) {
        fail(e);
      }
    },
    importPreset: async (path) => {
      try {
        const chain = presetChain(await api.readPresetFile(path), get().extensions);
        await get().refreshPresets();
        const name = presetNameFor(stemOf(path), get().presets);
        await api.savePreset(name, chain);
        await get().refreshPresets();
        const prev = snapshot();
        replaceChain(chain, name, signature(chain));
        say(`Imported “${name}”`, prev);
      } catch (e) {
        say(e instanceof Error ? e.message : String(e));
      }
    },
    deletePreset: async (name) => {
      try {
        await api.deletePreset(name);
        if (get().activePreset === name) {
          set({ activePreset: null, presetBaseline: null });
          persistNow();
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
