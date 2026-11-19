/**
 * The one-line summaries under the navigator's three section rows. They read what the store already holds and never the
 * camera, so showing them never lights its LED. No DOM and no store here, so `navSummary.test.ts` can run them in node.
 */
import { HIGHEST_FPS, MATCH_OUTPUT_FPS, fpsLabel, formatLabel, type CameraInfo, type OutputCfg, type SourceCfg, type StoredDevice } from "../camera/schema";

/** "1920x1080" as "1920×1080"; anything else as it is. */
const sizeText = (size: string) => size.replace(/^(\d+)x(\d+)$/, "$1×$2");

/** What the capture frame rate says: a rate, or the two entries that are not one. */
const rateText = (interval: number) => (interval === MATCH_OUTPUT_FPS ? "Match output FPS" : interval === HIGHEST_FPS ? "Highest FPS" : `${fpsLabel(interval)} fps`);

/**
 * The Camera row: `1920×1080 · 30 fps · MJPEG` when the size and rate are the user's choice (the format only when it is
 * not "any"), else `Device default`; a line about the camera itself while it is not there to describe.
 */
export function cameraSummary(source: SourceCfg, cameras: readonly CameraInfo[] | null, cam: CameraInfo | null): string {
  if (!cameras) return "Looking for cameras…";
  if (!cam) return source.video_device_id ? "Saved camera disconnected" : "No camera found";
  if (source.res_type !== "custom") return "Device default";
  return [sizeText(source.resolution), rateText(source.frame_interval), source.video_format === "any" ? null : formatLabel(source.video_format)]
    .filter((p) => p !== null)
    .join(" · ");
}

/** The Camera controls row: `Defaults`, or `N changed`, with ` · Better Cam auto` while a control is in that mode. */
export function controlsSummary(cam: CameraInfo | null, device: StoredDevice | null): string {
  if (!cam) return "No camera";
  const values = Object.values(device?.values ?? {});
  if (values.length === 0) return "Defaults";
  const auto = values.some((v) => v?.mode === "app");
  return `${values.length} changed${auto ? " · Better Cam auto" : ""}`;
}

const FIT_TEXT: Record<OutputCfg["fit"], string> = { fit: "Fit", fill: "Fill", stretch: "Stretch" };

/** The Output row: `Same as camera`, `Same as camera · 30 fps` or `1280×720 · Fill`. */
export function outputSummary(output: OutputCfg): string {
  const sized = output.resolution !== "source";
  return [sized ? sizeText(output.resolution) : "Same as camera", output.fps === "source" ? null : `${output.fps} fps`, sized ? FIT_TEXT[output.fit] : null]
    .filter((p) => p !== null)
    .join(" · ");
}
