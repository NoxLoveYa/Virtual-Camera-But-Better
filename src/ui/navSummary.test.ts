/**
 * Checks of the navigator's section summaries. `npm run test:ui` bundles and runs it. It throws at the first failure.
 */
import { DEFAULT_OUTPUT, DEFAULT_SOURCE, HIGHEST_FPS, MATCH_OUTPUT_FPS, type CameraInfo, type SourceCfg, type StoredDevice } from "../camera/schema";
import { cameraSummary, controlsSummary, outputSummary } from "./navSummary";

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

console.log("ok");
