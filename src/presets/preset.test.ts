/**
 * Checks of the version 2 preset file: parsing every shape of file, the round trip, the signature, the control plan and
 * the camera a preset names. `npm run test:preset` bundles and runs it. It throws at the first failure and prints `ok`
 * otherwise.
 */
import {
  CONTROL_KEYS,
  DEFAULT_AUTO,
  DEFAULT_OUTPUT,
  DEFAULT_SOURCE,
  type CameraInfo,
  type ControlInfo,
  type ControlKey,
  type StoredControls,
} from "../camera/schema";
import { makeFilter } from "../filters/schema";
import { signature } from "../filters/chain";
import {
  defaultState,
  isDefaultPreset,
  migrateBaseline,
  parsePreset,
  planControls,
  planNotes,
  presetCamera,
  presetFileOf,
  presetFileOfParsed,
  presetMatches,
  presetNotes,
  presetSignature,
  presetStateOf,
  withPresetCamera,
  type PresetCamera,
  type PresetState,
} from "./preset";

const same = (got: unknown, want: unknown, what: string) => {
  const a = JSON.stringify(got);
  const b = JSON.stringify(want);
  if (a !== b) throw new Error(`${what}: got ${a}, wanted ${b}`);
};
const differs = (a: unknown, b: unknown, what: string) => {
  if (JSON.stringify(a) === JSON.stringify(b)) throw new Error(`${what}: both are ${JSON.stringify(a)}`);
};
const throws = (f: () => unknown, text: string, what: string) => {
  try {
    f();
  } catch (e) {
    if (!String(e).includes(text)) throw new Error(`${what}: threw ${String(e)}, wanted "${text}"`);
    return;
  }
  throw new Error(`${what}: did not throw`);
};

const { video_device_id: _, ...DEFAULT_CAPTURE } = DEFAULT_SOURCE;
const BRIO = "\\\\?\\usb#vid_046d&pid_085e&mi_00#7&1a2b3c&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global";
const BRIO_OTHER_PORT = "\\\\?\\usb#vid_046d&pid_085e&mi_00#8&ffeedd&0&0001#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global";
const OTHER = "\\\\?\\usb#vid_1234&pid_5678&mi_00#7&aa&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global";

// --- parsePreset ---------------------------------------------------------------------------------------------------

const entry = { kind: "color_filter", enabled: true, params: { saturation: 0.2 } };

const bare = parsePreset([entry], []);
same([bare.version, bare.chain.map((f) => [f.kind, f.enabled, f.params.saturation]), bare.camera, bare.output], [1, [["color_filter", true, 0.2]], null, null], "bare array");
if (!bare.chain[0].id) throw new Error("bare array: a chain entry gets an id");

const v1 = parsePreset({ app: "better-cam", version: 1, chain: [entry] }, []);
same([v1.version, v1.chain.length, v1.camera, v1.output], [1, 1, null, null], "v1 object");

// The camera key of an old Rust test: not a camera section, so no camera.
const oldCamera = parsePreset({ app: "better-cam", version: 1, chain: [entry], camera: { gain: 3 } }, []);
same([oldCamera.chain.length, oldCamera.camera], [1, null], "v1 object with an unknown camera key");
same(parsePreset({ chain: [], camera: { capture: "no" } }, []).camera, null, "a capture that is not an object is no camera");
same(parsePreset({ chain: [], camera: [] }, []).camera, null, "a camera array is no camera");

const full = {
  app: "better-cam",
  version: 2,
  chain: [entry, { kind: "sharpness_filter", enabled: false, params: {} }],
  camera: {
    device: { id: BRIO, name: "Logitech BRIO" },
    capture: {
      res_type: "custom", resolution: "1920x1080", frame_interval: 333333, video_format: "mjpeg", color_space: "709",
      color_range: "full", buffering: "off", flip_vertically: true, autorotation: false, hw_decode: false, deactivate_when_not_showing: false,
    },
    controls: {
      // In CONTROL_KEYS order, which is the order the parser writes them in.
      brightness: { value: 140, auto: false },
      gain: { value: 40, auto: false, mode: "app" },
      exposure: { value: -6, auto: false, mode: "app" },
      focus: { value: 30, auto: false },
    },
    auto: {
      exposure: { target: 0.5, longest: -4, max_gain: 60, speed: "fast" },
      white_balance: { warmth: 0.1, speed: "slow" },
    },
  },
  output: { resolution: "1280x720", fps: 30, fit: "fill", background_color: 4278190080, extra_sizes: false },
};
const v2 = parsePreset(full, []);
same(v2.version, 2, "v2 version");
same(v2.chain.map((f) => f.kind), ["color_filter", "sharpness_filter"], "v2 chain");
same(v2.camera, full.camera, "v2 camera is read as written");
same(v2.output, full.output, "v2 output is read as written");

// Junk in every field: each falls back to its default.
const junk = parsePreset(
  {
    app: 5,
    version: "two",
    chain: [entry, 7, { kind: "nope", enabled: true, params: {} }],
    camera: {
      device: { id: 5, name: 6 },
      capture: {
        res_type: "x", resolution: 5, frame_interval: "fast", video_format: "bmp", color_space: "p3", color_range: 1,
        buffering: null, flip_vertically: "yes", autorotation: 0, hw_decode: [], deactivate_when_not_showing: {},
      },
      controls: {
        brightness: "x", contrast: { value: 200, auto: "yes" }, exposure: { value: 1.5, auto: false },
        gain: { value: 3, auto: false, mode: "app" }, hue: { value: 2, auto: false, mode: "app" }, nope: { value: 1, auto: false },
      },
      auto: 7,
    },
    output: { resolution: 5, fps: "x", fit: "tile", background_color: "red", extra_sizes: "no" },
  },
  [],
);
same(junk.version, 1, "junk version");
same(junk.chain.map((f) => f.kind), ["color_filter"], "junk chain keeps the valid entry");
same(junk.camera, { device: null, capture: DEFAULT_CAPTURE, controls: { hue: { value: 2, auto: false }, gain: { value: 3, auto: false } }, auto: DEFAULT_AUTO }, "junk camera (mode app is dropped outside exposure, white balance and gain; a gain without exposure app is manual)");
same(junk.output, DEFAULT_OUTPUT, "junk output");
same(parsePreset({ chain: [], camera: { capture: {} }, output: {} }, []).camera?.capture, DEFAULT_CAPTURE, "an empty capture is the defaults");
same(parsePreset({ chain: [], output: "no" }, []).output, null, "an output that is not an object is no output");
same(parsePreset({ chain: [], output: {} }, []).output, DEFAULT_OUTPUT, "an empty output is the defaults");
// The device id is kept as it is; a name that is not text is empty.
same(parsePreset({ chain: [], camera: { capture: {}, device: { id: BRIO, name: 3 } } }, []).camera?.device, { id: BRIO, name: "" }, "device name");

throws(() => parsePreset("text", []), "This is not a Better Cam preset", "text");
throws(() => parsePreset(null, []), "This is not a Better Cam preset", "null");
throws(() => parsePreset({ app: "better-cam" }, []), "This is not a Better Cam preset", "no chain");
throws(() => parsePreset({ chain: "no" }, []), "This is not a Better Cam preset", "chain that is not a list");

// A newer version: the sections it has are read, the version is reported.
const v3 = parsePreset({ ...full, version: 3, futureSection: { x: 1 } }, []);
same([v3.version, v3.chain.length, v3.camera?.device?.name, v3.output?.fps], [3, 2, "Logitech BRIO", 30], "version 3");

// --- presetFileOf round trip --------------------------------------------------------------------------------------

const state: PresetState = {
  chain: [makeFilter("color_filter"), { ...makeFilter("sharpness_filter"), enabled: false }],
  camera: v2.camera!,
  output: v2.output!,
};
const file = presetFileOf(state);
same([file.app, file.version], ["better-cam", 2], "file header");
same(file.chain.map((e) => Object.keys(e).sort()), [["enabled", "kind", "params"], ["enabled", "kind", "params"]], "file chain has no ids");
const back = parsePreset(JSON.parse(JSON.stringify(file)), []);
same(back.version, 2, "round trip version");
same(back.chain.map(({ id: _id, ...rest }) => rest), state.chain.map(({ id: _id, ...rest }) => rest), "round trip chain");
same(back.camera, state.camera, "round trip camera");
same(back.output, state.output, "round trip output");
if (back.chain[0].id === state.chain[0].id) throw new Error("round trip: ids are fresh");
same(parsePreset(JSON.parse(JSON.stringify(presetFileOf(defaultState(null)))), []).camera, defaultState(null).camera, "round trip of the defaults");

// --- presetSignature ----------------------------------------------------------------------------------------------

const clone = (s: PresetState): PresetState => JSON.parse(JSON.stringify(s));
const sig = presetSignature(state);
if (!sig.startsWith("v2:")) throw new Error("signature starts with v2:");
same(presetSignature(clone(state)), sig, "a copy has the same signature");

const fresh = clone(state);
fresh.chain = fresh.chain.map((f, i) => ({ ...f, id: `other-${i}` }));
same(presetSignature(fresh), sig, "ids are ignored");

const app = clone(state);
app.camera.controls.exposure = { value: -3, auto: false, mode: "app" };
app.camera.controls.gain = { value: 70, auto: false, mode: "app" };
same(presetSignature(app), sig, "the value of an app control is ignored");

const manual = clone(state);
manual.camera.controls.brightness = { value: 141, auto: false };
differs(presetSignature(manual), sig, "a manual value change counts");
const autoFlag = clone(state);
autoFlag.camera.controls.brightness = { value: 140, auto: true };
differs(presetSignature(autoFlag), sig, "an Auto flag change counts");
const appOff = clone(state);
appOff.camera.controls.exposure = { value: -6, auto: false };
differs(presetSignature(appOff), sig, "leaving Better Cam auto counts");

const port = clone(state);
port.camera.device = { id: BRIO_OTHER_PORT, name: "LOGITECH BRIO" };
same(presetSignature(port), sig, "the same camera on another port is the same");
const other = clone(state);
other.camera.device = { id: OTHER, name: "Logitech BRIO" };
differs(presetSignature(other), sig, "another camera counts");
const noCam = clone(state);
noCam.camera.device = null;
differs(presetSignature(noCam), sig, "no camera counts");

const pref = clone(state);
pref.camera.capture.res_type = "preferred";
const prefSig = presetSignature(pref);
differs(prefSig, sig, "Custom to Device default counts");
pref.camera.capture.resolution = "640x480";
pref.camera.capture.frame_interval = 666666;
same(presetSignature(pref), prefSig, "resolution and frame interval are ignored under Device default");
const custom = clone(state);
custom.camera.capture.resolution = "1280x720";
differs(presetSignature(custom), sig, "resolution counts under Custom");
const flip = clone(state);
flip.camera.capture.flip_vertically = false;
differs(presetSignature(flip), sig, "a capture field counts");

const tune = clone(state);
tune.camera.auto.exposure.target = 0.6;
differs(presetSignature(tune), sig, "Better Cam auto settings count");
const out = clone(state);
out.output.fit = "stretch";
differs(presetSignature(out), sig, "the output counts");
const chainEdit = clone(state);
chainEdit.chain[0].params.saturation = 1;
differs(presetSignature(chainEdit), sig, "the chain counts");

// --- planControls -------------------------------------------------------------------------------------------------

const info = (key: ControlKey, over: Partial<ControlInfo> = {}): ControlInfo => ({
  key, min: 0, max: 255, step: 1, def: 128, value: 128, auto: false, can_auto: false, can_manual: true, ...over,
});
const brightness = (over: Partial<ControlInfo> = {}) => info("brightness", over);
const focus = (over: Partial<ControlInfo> = {}) => info("focus", { def: 0, value: 0, auto: true, can_auto: true, mode: "camera", ...over });
const exposure = (over: Partial<ControlInfo> = {}) =>
  info("exposure", { min: -11, max: -2, def: -6, value: -6, auto: true, can_auto: true, mode: "camera", app: "full", ...over });
const gain = (over: Partial<ControlInfo> = {}) => info("gain", { min: 0, max: 100, def: 0, value: 0, ...over });
const whiteBalance = (over: Partial<ControlInfo> = {}) =>
  info("white_balance", { min: 2000, max: 6500, def: 4000, value: 4000, auto: true, can_auto: true, mode: "camera", app: "full", ...over });

const plan = (infos: ControlInfo[], target: PresetCamera["controls"]) => planControls(infos, target);

// The default target resets a changed Brightness and turns Focus Auto back on.
const reset = plan([brightness({ value: 140 }), focus({ auto: false, value: 30, mode: "manual" }), exposure(), whiteBalance(), gain()], {});
same(reset.a, [], "reset: phase A");
same(reset.b, [
  { key: "brightness", op: "set", value: 128, auto: false },
  { key: "focus", op: "set", value: 0, auto: true },
], "reset: phase B");
same([reset.c, reset.skipped, reset.downgraded], [[], [], []], "reset: nothing else");
// Nothing to do when the camera is in its default state.
same(plan([brightness(), focus(), exposure(), whiteBalance(), gain()], {}), { a: [], b: [], c: [], skipped: [], downgraded: [] }, "default state needs no writes");

// Exposure in Better Cam auto, wanted Manual: the hand-back is phase A; the gain it drove follows with the exposure.
const leave = plan(
  [exposure({ auto: false, mode: "app", value: -4 }), gain({ mode: "app", value: 40 }), whiteBalance()],
  { exposure: { value: -6, auto: false } },
);
same(leave.a, [{ key: "exposure", op: "mode", mode: "manual" }], "leave app: phase A");
same(leave.b, [{ key: "gain", op: "set", value: 0, auto: false }, { key: "exposure", op: "set", value: -6, auto: false }], "leave app: phase B");
same(leave.c, [], "leave app: phase C");
// Wanted Camera auto after Better Cam auto: one mode step, no second Auto write.
const toCamera = plan([exposure({ auto: false, mode: "app", value: -4 }), gain({ mode: "app", value: 0 })], {});
same(toCamera, { a: [{ key: "exposure", op: "mode", mode: "camera" }], b: [], c: [], skipped: [], downgraded: [] }, "app to Camera auto");

// Exposure wanted app: no exposure or gain step in phase B, one mode app step in phase C. White balance comes after it.
const engage = plan(
  [whiteBalance({ value: 4500 }), exposure(), gain({ value: 40 })],
  {
    white_balance: { value: 5000, auto: false, mode: "app" },
    exposure: { value: -6, auto: false, mode: "app" },
    gain: { value: 40, auto: false, mode: "app" },
  },
);
same(engage.a, [], "engage: phase A");
same(engage.b, [], "engage: no exposure, gain or white balance step in phase B");
same(engage.c, [{ key: "exposure", op: "mode", mode: "app" }, { key: "white_balance", op: "mode", mode: "app" }], "engage: exposure, then white balance");
// Already in Better Cam auto: nothing to write.
same(
  plan([exposure({ auto: false, mode: "app" }), gain({ mode: "app" })], { exposure: { value: -6, auto: false, mode: "app" }, gain: { value: 0, auto: false, mode: "app" } }),
  { a: [], b: [], c: [], skipped: [], downgraded: [] },
  "already in Better Cam auto",
);
// A gain stored as app without the exposure being app is Manual.
const orphan = plan([exposure(), gain({ value: 10 })], { gain: { value: 30, auto: false, mode: "app" } });
same(orphan.b, [{ key: "gain", op: "set", value: 30, auto: false }], "gain app without exposure app is manual");

// Better Cam auto wanted where the camera offers none: Camera auto if it has Auto, else Manual at the stored value.
const noApp = plan(
  [exposure({ app: null, auto: false, mode: "manual", value: -3 }), whiteBalance({ app: undefined, can_auto: false, auto: false, mode: "manual", value: 5000 })],
  { exposure: { value: -6, auto: false, mode: "app" }, white_balance: { value: 4300, auto: false, mode: "app" } },
);
same(noApp.downgraded, ["exposure", "white_balance"], "downgraded keys");
same(noApp.b, [{ key: "white_balance", op: "set", value: 4300, auto: false }, { key: "exposure", op: "set", value: -6, auto: true }], "downgraded: Manual or Camera auto");
same(noApp.c, [], "downgraded: nothing engaged");

// A missing key and a key without a manual mode are skipped; values are clamped.
const edge = plan(
  [brightness({ value: 100 }), info("pan", { auto: true, can_auto: true, can_manual: false, mode: "camera" }), info("zoom", { min: 100, max: 500, def: 100, value: 100 })],
  {
    brightness: { value: 999, auto: false },
    pan: { value: 5, auto: false },
    roll: { value: 1, auto: false },
    zoom: { value: 50, auto: false },
  },
);
same(edge.skipped, [{ key: "roll", why: "not on this camera" }, { key: "pan", why: "no manual mode" }], "skipped keys");
same(edge.b, [{ key: "brightness", op: "set", value: 255, auto: false }], "clamped to the maximum; zoom is already at its minimum");
same(plan([zoomLow()], { zoom: { value: 0, auto: false } }).b, [{ key: "zoom", op: "set", value: 100, auto: false }], "clamped to the minimum");
function zoomLow() {
  return info("zoom", { min: 100, max: 500, def: 300, value: 300 });
}
// A key with no manual mode that already is where the preset wants it is not reported.
same(plan([info("pan", { auto: true, can_auto: true, can_manual: false, mode: "camera" })], {}).skipped, [], "no skip when nothing differs");
// Steps come in CONTROL_KEYS order.
const order = plan(CONTROL_KEYS.map((k) => info(k, { value: 7, def: 9, min: 0, max: 20 })), {});
same(order.b.map((s) => s.key), CONTROL_KEYS, "phase B follows CONTROL_KEYS order");

// --- presetCamera -------------------------------------------------------------------------------------------------

const cam = (id: string, name: string): CameraInfo => ({ id, name, formats: [] });
const brio = cam(BRIO, "Logitech BRIO");
same(presetCamera([cam(OTHER, "Other"), brio], { id: BRIO, name: "Logitech BRIO" })?.id, BRIO, "exact link");
same(presetCamera([cam(OTHER, "Other"), cam(BRIO_OTHER_PORT, "Logitech BRIO")], { id: BRIO, name: "Logitech BRIO" })?.id, BRIO_OTHER_PORT, "another port");
same(presetCamera([cam(OTHER, "Other")], { id: BRIO, name: "Logitech BRIO" }), null, "a missing camera");
same(presetCamera([brio], null), null, "no camera in the preset");
same(presetCamera([], { id: BRIO, name: "Logitech BRIO" }), null, "no camera connected");
// Two cameras of one model on other ports: the name alone cannot tell which, so none is chosen.
const twinA = "\\\\?\\usb#vid_046d&pid_085e&mi_00#9&1&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global";
const twinB = "\\\\?\\usb#vid_046d&pid_085e&mi_00#9&2&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global";
same(presetCamera([cam(twinA, "Logitech BRIO"), cam(twinB, "Logitech BRIO")], { id: BRIO, name: "Logitech BRIO" }), null, "two identical cameras");
// A camera without a vid/pid in its id (a virtual one) is found by name.
same(presetCamera([cam("@device:sw:virtual-b", "OBS Virtual Camera"), cam("@device:sw:virtual-a", "Other")], { id: "@device:sw:old-id", name: "obs virtual camera" })?.id, "@device:sw:virtual-b", "by name when there is no vid/pid");

// --- defaultState -------------------------------------------------------------------------------------------------

const wide = (id: string): CameraInfo => ({
  id,
  name: "Cam",
  formats: [
    { format: "yuy2", width: 640, height: 480, intervals: [333333] },
    { format: "mjpeg", width: 1920, height: 1080, intervals: [333333] },
  ],
});
const def = defaultState(wide(BRIO));
same(def.chain, [], "default chain");
same(def.camera.device, { id: BRIO, name: "Cam" }, "default device");
same(def.camera.capture, { ...DEFAULT_CAPTURE, resolution: "1920x1080" }, "default capture");
same([def.camera.controls, def.camera.auto, def.output], [{}, DEFAULT_AUTO, DEFAULT_OUTPUT], "default controls, auto and output");
same(defaultState(null).camera.device, null, "default state without a camera");
same(defaultState(null).camera.capture, DEFAULT_CAPTURE, "default capture without a camera");
// Two calls do not share objects.
defaultState(null).camera.auto.exposure.target = 0.8;
same(defaultState(null).camera.auto, DEFAULT_AUTO, "defaults are fresh");

// --- what the store holds as a preset state (P2) -----------------------------------------------------------------------

const stored: StoredControls = {
  [BRIO]: {
    name: "Logitech BRIO",
    values: { brightness: { value: 140, auto: false }, exposure: { value: -6, auto: false, mode: "app" } },
    auto: { ...DEFAULT_AUTO, exposure: { ...DEFAULT_AUTO.exposure, target: 0.6 } },
    learned: { gain_stops: 3 },
  },
};
const src = { ...DEFAULT_SOURCE, video_device_id: BRIO_OTHER_PORT, flip_vertically: true };
const now = presetStateOf([], src, DEFAULT_OUTPUT, stored, [cam(BRIO_OTHER_PORT, "Logitech BRIO")]);
same(now.camera.device, { id: BRIO_OTHER_PORT, name: "Logitech BRIO" }, "state: the connected camera, on its new port");
same([now.camera.controls, now.camera.auto.exposure.target, now.camera.capture.flip_vertically], [stored[BRIO].values, 0.6, true], "state: the record of the moved camera");
const gone = presetStateOf([], { ...DEFAULT_SOURCE, video_device_id: BRIO }, DEFAULT_OUTPUT, stored, []);
same(gone.camera.device, { id: BRIO, name: "Logitech BRIO" }, "state: a saved camera that is not connected keeps its name from the record");
same(presetStateOf([], DEFAULT_SOURCE, DEFAULT_OUTPUT, {}, null).camera, defaultState(null).camera, "state: no camera, nothing stored");

// Default, whatever the camera.
if (!isDefaultPreset(defaultState(wide(BRIO)))) throw new Error("the defaults for a camera are the defaults");
if (!isDefaultPreset(presetStateOf([], DEFAULT_SOURCE, DEFAULT_OUTPUT, {}, [wide(BRIO)]))) throw new Error("a fresh install is the defaults");
if (isDefaultPreset(presetStateOf([makeFilter("color_filter")], DEFAULT_SOURCE, DEFAULT_OUTPUT, {}, null))) throw new Error("a filter is not the defaults");
if (isDefaultPreset(presetStateOf([], DEFAULT_SOURCE, { ...DEFAULT_OUTPUT, resolution: "1280x720" }, {}, null))) throw new Error("an output change is not the defaults");
if (isDefaultPreset(now)) throw new Error("changed controls are not the defaults");

// The record a load makes: values replaced, Better Cam auto settings replaced, learned values kept; app entries as Manual when they are to be engaged later.
const seeded = withPresetCamera(stored, { id: BRIO, name: "Logitech BRIO" }, { focus: { value: 30, auto: false }, exposure: { value: -4, auto: false, mode: "app" } }, DEFAULT_AUTO, true);
same(seeded[BRIO].values, { focus: { value: 30, auto: false }, exposure: { value: -4, auto: false } }, "record: values replaced, app seeded as Manual");
same([seeded[BRIO].auto, seeded[BRIO].learned], [undefined, { gain_stops: 3 }], "record: default Better Cam auto is not kept, learned stays");
same(withPresetCamera({}, { id: BRIO, name: "x" }, { exposure: { value: -4, auto: false, mode: "app" } }, DEFAULT_AUTO, false)[BRIO].values.exposure, { value: -4, auto: false, mode: "app" }, "record: app kept when not seeded");
same(withPresetCamera(stored, { id: BRIO, name: "Logitech BRIO" }, {}, DEFAULT_AUTO, true)[BRIO], { name: "Logitech BRIO", values: {}, learned: { gain_stops: 3 } }, "record: reset keeps only what was learned");
same(Object.keys(withPresetCamera({ [BRIO]: { name: "x", values: { focus: { value: 1, auto: false } } } }, { id: BRIO, name: "x" }, {}, DEFAULT_AUTO, true)), [], "record: an empty record is dropped");

// Baselines: a chain fingerprint from the filters-only build.
const chainOnly = [makeFilter("color_filter")];
const stateWith = { ...defaultState(null), chain: chainOnly };
same(migrateBaseline(signature(chainOnly), stateWith), presetSignature(stateWith), "old baseline that fits the chain becomes a full one");
same(migrateBaseline(signature([]), stateWith), null, "old baseline that does not fit is dropped");
same(migrateBaseline("v2:[]", stateWith), "v2:[]", "a v2 baseline stays");
same(migrateBaseline(null, stateWith), null, "no baseline");

// A saved preset matches the state it describes: a v2 file on everything, a v1 file on the filters.
const asFile = (s: PresetState) => parsePreset(JSON.parse(JSON.stringify(presetFileOf(s))), []);
if (!presetMatches(asFile(state), state)) throw new Error("a v2 file matches its own state");
if (presetMatches(asFile(state), manual)) throw new Error("a v2 file does not match a changed control");
const v1file = parsePreset([entry], []);
if (!presetMatches(v1file, { ...state, chain: v1file.chain })) throw new Error("a v1 file matches on its filters");
if (presetMatches(v1file, state)) throw new Error("a v1 file does not match other filters");

// A parsed file is written back as it was.
same(presetFileOfParsed(v1file).version, 1, "v1 stays v1");
same(Object.keys(presetFileOfParsed(v1file)).sort(), ["app", "chain", "version"], "v1 file has no camera or output");
const onlyOutput = parsePreset({ chain: [], output: { fps: 30 } }, []);
same([presetFileOfParsed(onlyOutput).version, "camera" in presetFileOfParsed(onlyOutput), presetFileOfParsed(onlyOutput).output?.fps], [2, false, 30], "a file with an output only keeps just that");
same(presetFileOfParsed(v2), presetFileOf({ chain: v2.chain, camera: v2.camera!, output: v2.output! }), "v2 is written whole");

// Toast notes.
same(presetNotes(v1file, "Old"), ["“Old” has filters only (saved by an older Better Cam)."], "note for an old file");
same(presetNotes(v2, "New"), [], "no note for a full file");
same(presetNotes(v3, "Newer"), ["This preset was saved by a newer Better Cam; some settings may be missing."], "note for a newer file");
same(
  planNotes({ a: [], b: [], c: [], skipped: [{ key: "roll", why: "not on this camera" }, { key: "pan", why: "no manual mode" }], downgraded: ["exposure"] }),
  ["2 camera controls are not on this camera.", "Better Cam auto is not available for Exposure on this camera."],
  "plan notes",
);
same(planNotes({ a: [], b: [], c: [], skipped: [{ key: "roll", why: "x" }], downgraded: [] }), ["1 camera control is not on this camera."], "plan note, one control");
same(planNotes({ a: [], b: [], c: [], skipped: [], downgraded: [] }), [], "no plan notes");

console.log("ok");
