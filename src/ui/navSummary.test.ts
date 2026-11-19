/**
 * Checks of the navigator's section summaries. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { DEFAULT_OUTPUT, DEFAULT_SOURCE, HIGHEST_FPS, MATCH_OUTPUT_FPS, type CameraInfo, type SourceCfg, type StoredDevice } from "../camera/schema";
import type { ExtensionInfo } from "../filters/schema";
import { cameraDot, cameraSummary, controlsDot, controlsSummary, extNoteOf, needsOf, outputSummary, rowStatus, type LoopState } from "./navSummary";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}

const cam: CameraInfo = { id: "cam1", name: "Logitech BRIO", formats: [] };
const custom: SourceCfg = { ...DEFAULT_SOURCE, video_device_id: "cam1", res_type: "custom", resolution: "1920x1080", frame_interval: 333333, video_format: "mjpeg" };

// Camera
eq(cameraSummary(custom, null, null), "Looking for cameras…", "cameras not read yet");
eq(cameraSummary(DEFAULT_SOURCE, [], null), "No camera found", "no camera at all");
eq(cameraSummary({ ...DEFAULT_SOURCE, video_device_id: "gone" }, [cam], null), "Saved camera disconnected", "the saved camera is not connected");
eq(cameraSummary({ ...DEFAULT_SOURCE, video_device_id: "cam1" }, [cam], cam), "Device default", "device default");
eq(cameraSummary(custom, [cam], cam), "1920×1080 · 30 fps · MJPEG", "custom size, rate and format");
eq(cameraSummary({ ...custom, video_format: "any" }, [cam], cam), "1920×1080 · 30 fps", "format any is left out");
eq(cameraSummary({ ...custom, frame_interval: 333667 }, [cam], cam), "1920×1080 · 29.97 fps · MJPEG", "a fractional rate");
eq(cameraSummary({ ...custom, frame_interval: MATCH_OUTPUT_FPS }, [cam], cam), "1920×1080 · Match output FPS · MJPEG", "match output");
eq(cameraSummary({ ...custom, frame_interval: HIGHEST_FPS, video_format: "any" }, [cam], cam), "1920×1080 · Highest FPS", "highest");

// Camera controls
const device = (values: StoredDevice["values"]): StoredDevice => ({ name: "Logitech BRIO", values });
eq(controlsSummary(null, null), "No camera", "no camera");
eq(controlsSummary(cam, null), "Defaults", "no record");
eq(controlsSummary(cam, device({})), "Defaults", "an empty record");
eq(controlsSummary(cam, device({ brightness: { value: 140, auto: false } })), "1 changed", "one control");
eq(controlsSummary(cam, device({ brightness: { value: 140, auto: false }, focus: { value: 30, auto: false } })), "2 changed", "two controls");
eq(
  controlsSummary(cam, device({ brightness: { value: 140, auto: false }, exposure: { value: -6, auto: false, mode: "app" } })),
  "2 changed · Better Cam auto",
  "a loop in Better Cam auto mode",
);

// Output
eq(outputSummary(DEFAULT_OUTPUT), "Same as camera", "default output");
eq(outputSummary({ ...DEFAULT_OUTPUT, fps: 30 }), "Same as camera · 30 fps", "camera size, 30 fps");
eq(outputSummary({ ...DEFAULT_OUTPUT, resolution: "1280x720", fit: "fill" }), "1280×720 · Fill", "sized, fill");
eq(outputSummary({ ...DEFAULT_OUTPUT, resolution: "1280x720", fps: 60, fit: "stretch" }), "1280×720 · 60 fps · Stretch", "sized, rate, stretch");
eq(outputSummary({ ...DEFAULT_OUTPUT, resolution: "1280x720" }), "1280×720 · Fit", "sized, fit");

// ---- status dots ----
const dot = (d: { tone: string; text: string } | null) => (d ? d.tone : "none");

// Camera: a warning for a camera that is gone or a format fallback, nothing otherwise
eq(dot(cameraDot({ video_device_id: "cam1" }, [cam], cam, null)), "none", "camera: fine");
eq(dot(cameraDot({ video_device_id: null }, [], null, null)), "none", "camera: none chosen and none found is not a warning");
eq(dot(cameraDot({ video_device_id: "gone" }, [cam], null, null)), "warn", "camera: the saved camera is disconnected");
eq(dot(cameraDot({ video_device_id: "gone" }, null, null, null)), "none", "camera: not read yet");
eq(dot(cameraDot({ video_device_id: "cam1" }, [cam], cam, { asked: "1920×1080 YUY2 60 fps", source: { res_type: "preferred", video_format: "any", resolution: "1280x720", frame_interval: 333333 } })), "warn", "camera: a fallback runs");
eq(cameraDot({ video_device_id: "cam1" }, [cam], cam, { asked: "1920×1080 YUY2 60 fps", source: { res_type: "preferred", video_format: "any", resolution: "1280x720", frame_interval: 333333 } })!.text.includes("1920×1080 YUY2 60 fps"), true, "camera: the fallback names what was asked");

// Camera controls: the worst tone of the loops that are in Better Cam auto
const app = (on = true) => {
  const mode = on ? ("app" as const) : undefined;
  return device({ exposure: { value: -6, auto: false, mode }, white_balance: { value: 4500, auto: false, mode } });
};
const loop = (state: LoopState["state"], message: string | null = null): LoopState => ({ state, message });
const none = { exposure: null, white_balance: null };
eq(dot(controlsDot(null, none, null)), "none", "controls: nothing");
eq(dot(controlsDot(app(), none, null)), "none", "controls: in auto mode without a status yet");
eq(dot(controlsDot(app(), { exposure: loop("holding"), white_balance: null }, null)), "neutral", "controls: working");
eq(dot(controlsDot(app(), { exposure: loop("limit_gain"), white_balance: null }, null)), "warn", "controls: a limit");
eq(dot(controlsDot(app(), { exposure: loop("holding"), white_balance: loop("at_limit") }, null)), "warn", "controls: the worse of two");
eq(dot(controlsDot(app(), { exposure: loop("limit_gain"), white_balance: loop("hunting") }, null)), "error", "controls: a stopped loop is an error");
eq(dot(controlsDot(app(false), { exposure: loop("limit_gain"), white_balance: null }, null)), "none", "controls: a loop that is not in auto mode is stale");
eq(dot(controlsDot(app(), { exposure: loop("holding"), white_balance: null }, "slow")), "warn", "controls: the slow-camera hint warns");
eq(dot(controlsDot(null, none, "slow")), "warn", "controls: the slow-camera hint warns without a loop");
eq(dot(controlsDot(app(), { exposure: loop("hunting"), white_balance: null }, "slow")), "error", "controls: the slow-camera hint does not hide an error");
eq(controlsDot(app(), { exposure: loop("limit_gain", "At the gain limit."), white_balance: null }, null)!.text, "At the gain limit.", "controls: the backend's text is the tooltip");

// Filter rows
const noExt = { needs: null, ext: null, error: undefined, loading: undefined };
eq(rowStatus(noExt), null, "row: nothing to say");
eq(JSON.stringify(rowStatus({ ...noExt, error: "could not load x.cube (0x80004005)" })), JSON.stringify({ kind: "dot", tone: "error", text: "could not load x.cube" }), "row: an error, codes stripped");
eq(JSON.stringify(rowStatus({ ...noExt, loading: "Loading model…" })), JSON.stringify({ kind: "loading", text: "Loading model…" }), "row: loading is a spinner");
eq(rowStatus({ ...noExt, error: "e", loading: "Loading model…" })!.kind, "dot", "row: an error beats loading");
eq(rowStatus({ ...noExt, needs: "Needs an NVIDIA RTX GPU.", error: "e" })!.text, "Needs an NVIDIA RTX GPU.", "row: what the filter needs comes first");
const toneOf = (s: ReturnType<typeof rowStatus>) => (s && s.kind === "dot" ? s.tone : "none");
eq(toneOf(rowStatus({ ...noExt, ext: { failed: false, text: "off" }, loading: "x" })), "warn", "row: an extension switched off is a warning");
eq(toneOf(rowStatus({ ...noExt, ext: { failed: true, text: "failed" } })), "error", "row: an extension that failed is an error");
eq(toneOf(rowStatus({ ...noExt, ext: { failed: false, text: "off" }, error: "e" })), "error", "row: an error beats a warning");
eq(extNoteOf("x", undefined)!.state, "missing", "extension: not installed");
eq(extNoteOf("x", { id: "x", name: "X", state: "disabled" } as ExtensionInfo)!.failed, false, "extension: switched off does not fail");
eq(extNoteOf("x", { id: "x", name: "X", state: "ready" } as ExtensionInfo), null, "extension: ready says nothing");
eq(needsOf(undefined, true, null, null), null, "needs: not an NVIDIA filter");
eq(needsOf(true, false, null, null), null, "needs: GPU not read yet");
eq(needsOf(true, true, null, null)!.install, false, "needs: no RTX GPU");
eq(needsOf(true, true, {}, { available: false, detail: "Runtime missing." })!.text, "Runtime missing.", "needs: the runtime is missing");
eq(needsOf(true, true, {}, { available: true, detail: "" }), null, "needs: all there");

console.log("ok");
