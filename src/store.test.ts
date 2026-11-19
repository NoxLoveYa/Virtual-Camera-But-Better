/**
 * The store's start and its first camera list (the factory start of a fresh install), with the backend stubbed: only a
 * launch with nothing saved is a fresh install and seeds the factory record; a camera that is busy at the first pick gets
 * the factory capture when it can be read; white balance is put in Better Cam auto only where the camera offers it.
 * `npm run test:preset` bundles and runs it.
 */
import {
  DEFAULT_OUTPUT,
  DEFAULT_SOURCE,
  FACTORY_AUTO,
  FACTORY_OUTPUT,
  FACTORY_SOURCE,
  repairSource,
  type CameraInfo,
  type ControlInfo,
  type StoredControls,
} from "./camera/schema";

const same = (got: unknown, want: unknown, what: string) => {
  const a = JSON.stringify(got);
  const b = JSON.stringify(want);
  if (a !== b) throw new Error(`${what}: got ${a}, wanted ${b}`);
};

// A page without a window: the store only needs a localStorage.
const mem = new Map<string, string>();
(globalThis as unknown as { localStorage: Storage }).localStorage = {
  getItem: (k: string) => mem.get(k) ?? null,
  setItem: (k: string, v: string) => void mem.set(k, v),
  removeItem: (k: string) => void mem.delete(k),
  clear: () => mem.clear(),
  key: () => null,
  length: 0,
};

const { api } = await import("./api");
const { loadSaved, useStore } = await import("./store");

// --- the start: only an empty storage is a fresh install -------------------------------------------------------------
const fresh = loadSaved();
same([fresh.factoryPending, fresh.factoryCapturePending], [true, false], "nothing saved: a fresh install");
same([fresh.source, fresh.output, fresh.chain.length, fresh.controls], [FACTORY_SOURCE, FACTORY_OUTPUT, 1, {}], "a fresh install starts from the factory defaults");

mem.set("better-cam", JSON.stringify({ source: { ...DEFAULT_SOURCE, video_device_id: null }, output: DEFAULT_OUTPUT, controls: {}, chain: [] }));
const upgraded = loadSaved();
same([upgraded.factoryPending, upgraded.factoryCapturePending], [false, false], "an existing save that never picked a camera is not a fresh install");
same([upgraded.source.res_type, upgraded.output.fit, upgraded.chain], ["preferred", "fit", []], "and keeps what it saved");
mem.set("better-cam", JSON.stringify({ source: DEFAULT_SOURCE, output: DEFAULT_OUTPUT, controls: {} }));
same(loadSaved().chain, [], "a save without a chain list starts with no filters, as before");
mem.set("better-cam", "not json");
const broken = loadSaved();
same([broken.factoryPending, broken.source.res_type, broken.output.fit, broken.chain], [false, "preferred", "fit", []], "corrupt data is not a fresh install and gets what it always got");
mem.set("better-cam", JSON.stringify({ factoryPending: true, factoryCapturePending: true, source: DEFAULT_SOURCE }));
same([loadSaved().factoryPending, loadSaved().factoryCapturePending], [true, true], "the flags are saved");
mem.delete("better-cam");

// --- the first camera list ----------------------------------------------------------------------------------------------
const mj = (width: number, height: number, ...intervals: number[]) => ({ format: "mjpeg", width, height, intervals });
const CAM = "\\\\?\\usb#vid_046d&pid_085e&mi_00#7&1&0&0000#{guid}\\global";
const readable: CameraInfo = { id: CAM, name: "Brio", formats: [mj(3840, 2160, 666667), mj(1920, 1080, 333333), mj(1280, 720, 333333)] };
const busy: CameraInfo = { id: CAM, name: "Brio", formats: [] };

const wb = (over: Partial<ControlInfo> = {}): ControlInfo => ({
  key: "white_balance", min: 2800, max: 6500, step: 1, def: 4600, value: 4600, auto: true, can_auto: true, can_manual: true, mode: "camera", app: "full", ...over,
});
let cameras: CameraInfo[] = [];
let controls: ControlInfo[] | Error = [wb()];
const sent: { source: unknown; record: unknown }[] = [];
const stub = api as unknown as Record<string, unknown>;
stub.listCameras = async () => cameras;
stub.getCameraControls = async () => {
  if (controls instanceof Error) throw controls;
  return controls;
};
stub.setSource = async (source: unknown, record: unknown) => void sent.push({ source, record });

const store = useStore;
const records = (): StoredControls => store.getState().controls;
const shape = () => {
  const s = store.getState().source;
  return [s.video_device_id, s.res_type, s.video_format, s.resolution, s.frame_interval];
};

// The store of this run began with nothing saved (the stub was empty): a fresh install.
same(store.getState().factoryPending, true, "the store started as a fresh install");

// No camera yet: nothing is picked and nothing is spent.
await store.getState().refreshCameras();
same([store.getState().source.video_device_id, store.getState().factoryPending], [null, true], "no camera: nothing picked, the factory record is still owed");

// A camera that is busy: it is picked, with the device default for now, and its controls cannot be read: nothing is seeded yet.
cameras = [busy];
controls = new Error("in use");
await store.getState().refreshCameras();
same(shape(), [CAM, "preferred", "any", FACTORY_SOURCE.resolution, 333333], "busy: the device default");
same([store.getState().factoryCapturePending, store.getState().factoryPending, records()], [true, true, {}], "busy: both are still owed");
await new Promise((r) => setTimeout(r, 450)); // the save is folded into one write every 300 ms
same(JSON.parse(mem.get("better-cam") ?? "{}").factoryCapturePending, true, "the debt is saved");

// The camera is free: the factory capture and, with white balance offered, the factory record.
cameras = [readable];
controls = [wb()];
await store.getState().refreshCameras();
same(shape(), [CAM, "custom", "mjpeg", "1920x1080", 333333], "readable later: the factory capture at the largest 16:9 size with 30 fps");
same(store.getState().source, repairSource(FACTORY_SOURCE, [readable]), "readable later: the same source as a first pick of a readable camera");
same([store.getState().factoryCapturePending, store.getState().factoryPending], [false, false], "readable later: nothing is owed any more");
const rec = records()[CAM];
same(rec.values, { white_balance: { auto: false, mode: "app" } }, "the record: white balance in Better Cam auto with no value, nothing else");
same(rec.auto, FACTORY_AUTO, "the record: the factory Better Cam auto settings");
same(sent.length > 0 && JSON.stringify(sent[sent.length - 1].record).includes('"mode":"app"'), true, "the backend is told of the record");

// Nothing is seeded a second time.
const before = JSON.stringify(records());
await store.getState().refreshCameras();
same(JSON.stringify(records()), before, "a later list changes nothing");

// A camera that does not offer Better Cam auto white balance gets the settings but no white balance entry.
store.setState({ controls: {}, source: { ...FACTORY_SOURCE }, factoryPending: true, factoryCapturePending: false });
controls = [wb({ app: null, app_why: "This camera reports white balance in units Better Cam does not know." })];
cameras = [readable];
await store.getState().refreshCameras();
same(records()[CAM].values, {}, "not offered: no white balance entry");
same(records()[CAM].auto, FACTORY_AUTO, "not offered: the factory Better Cam auto settings are still seeded");
same(store.getState().factoryPending, false, "not offered: spent");

// A camera that already has a record is left as it is.
const mine: StoredControls = { [CAM]: { name: "Brio", values: { brightness: { value: 9, auto: false } } } };
store.setState({ controls: mine, source: { ...FACTORY_SOURCE }, factoryPending: true });
await store.getState().refreshCameras();
same([records(), store.getState().factoryPending], [mine, false], "a camera with a record is not touched");

// An existing user who never selected a camera is not a fresh install: the first pick seeds nothing.
store.setState({ controls: {}, source: { ...DEFAULT_SOURCE }, factoryPending: false, factoryCapturePending: false });
controls = [wb()];
await store.getState().refreshCameras();
same([records(), store.getState().source.res_type, store.getState().source.video_device_id], [{}, "preferred", CAM], "not fresh: no record and the old first-pick source");

// A first pick that is busy is only owed the factory capture on a fresh install.
store.setState({ controls: {}, source: { ...DEFAULT_SOURCE }, factoryPending: false, factoryCapturePending: false });
cameras = [busy];
await store.getState().refreshCameras();
same(store.getState().factoryCapturePending, false, "not fresh: busy at the first pick owes nothing");

// An edit of the capture ends the debt: the user's choice is not replaced.
store.setState({ controls: {}, source: { ...FACTORY_SOURCE }, factoryPending: true, factoryCapturePending: false });
cameras = [busy];
controls = new Error("in use");
await store.getState().refreshCameras();
same(store.getState().factoryCapturePending, true, "owed again");
store.getState().patchSource({ video_format: "yuy2" });
same(store.getState().factoryCapturePending, false, "a capture edit ends the debt");
cameras = [readable];
await store.getState().refreshCameras();
same(store.getState().source.video_format === "mjpeg", false, "the user's format is not replaced by the factory one");

console.log("ok");
