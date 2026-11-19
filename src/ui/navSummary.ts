/**
 * The one-line summaries under the navigator's three section rows. They read what the store already holds and never the
 * camera, so showing them never lights its LED. No DOM and no store here, so `navSummary.test.ts` can run them in node.
 */
import {
  HIGHEST_FPS,
  MATCH_OUTPUT_FPS,
  autoTone,
  fpsLabel,
  formatLabel,
  stripCodes,
  type AutoState,
  type AutoTone,
  type CameraInfo,
  type InUse,
  type OutputCfg,
  type SourceCfg,
  type StoredDevice,
} from "../camera/schema";
import type { ExtensionInfo } from "../filters/schema";

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

// ---------------------------------------------------------------------------------------------------------------
// Status dots (docs/LAYOUT-INSPECTOR.md 1.5): what a row says at a glance. Each has a tone and the sentence for its tooltip.

/** The 8 px dot of a navigator row. `neutral` is the accent colour: something runs and is fine. */
export interface Dot {
  tone: AutoTone;
  text: string;
}

const RANK: Record<AutoTone, number> = { neutral: 0, warn: 1, error: 2 };

/** The Camera row: a warning while the saved camera is not connected, or while a format fallback runs. */
export function cameraDot(source: Pick<SourceCfg, "video_device_id">, cameras: readonly CameraInfo[] | null, cam: CameraInfo | null, inUse: InUse | null): Dot | null {
  if (cameras && source.video_device_id && !cam) return { tone: "warn", text: "The saved camera is not connected." };
  if (inUse) return { tone: "warn", text: `The camera could not deliver ${inUse.asked}, so its default runs.` };
  return null;
}

/** What one Better Cam auto loop reports (`ExposureStatus` and `WhiteBalanceStatus` both fit). */
export interface LoopState {
  state: AutoState;
  message: string | null;
}

const LOOP_NAME = { exposure: "exposure", white_balance: "white balance" } as const;

/**
 * The Camera controls row: the worst tone of the two Better Cam auto loops, for the loops that are in `app` mode and have a
 * status; a warning while the slow-camera hint holds (`slow` is its text) when nothing is worse.
 */
export function controlsDot(
  device: StoredDevice | null,
  loops: { exposure: LoopState | null; white_balance: LoopState | null },
  slow: string | null,
): Dot | null {
  let worst: Dot | null = null;
  for (const key of ["exposure", "white_balance"] as const) {
    const s = loops[key];
    if (!s || device?.values[key]?.mode !== "app") continue;
    const dot: Dot = { tone: autoTone(s.state), text: s.message?.trim() || `Better Cam auto ${LOOP_NAME[key]}: ${s.state.replace(/_/g, " ")}` };
    if (!worst || RANK[dot.tone] > RANK[worst.tone]) worst = dot;
  }
  if (slow && (!worst || worst.tone === "neutral")) return { tone: "warn", text: slow };
  return worst;
}

/** What a filter of an extension says when the extension is not running: not installed, failed to load, or switched off. */
export function extNoteOf(id: string, ext: ExtensionInfo | undefined) {
  if (!ext) return { state: "missing", failed: true, text: `Extension “${id}” is not installed. The filter is bypassed and keeps its settings.` };
  if (ext.state === "error") return { state: "error", failed: true, text: `Extension “${id}” failed to load: ${ext.error ?? "unknown error"}` };
  if (ext.state === "disabled") return { state: "disabled", failed: false, text: `Extension “${ext.name}” is switched off in Settings → Extensions. The filter is bypassed.` };
  return null;
}

/**
 * What an NVIDIA filter needs before it can run, in the order of precedence: an RTX GPU, then the runtime. The app knows both
 * without the engine, so a filter in a loaded preset says it before the first frame.
 */
export function needsOf(rtx: boolean | undefined, gpuRead: boolean, gpu: unknown, nvidia: { available: boolean; detail: string } | null): { text: string; install: boolean } | null {
  if (!rtx || !gpuRead) return null;
  if (!gpu) return { text: "Needs an NVIDIA RTX GPU (GeForce RTX 20 series or newer).", install: false };
  if (nvidia && !nvidia.available) return { text: nvidia.detail, install: true };
  return null;
}

/** A filter row's state: an error or warning dot with its message, or "loading" for the spinner; `null` when there is nothing to say. */
export type RowStatus = { kind: "dot"; tone: "error" | "warn"; text: string } | { kind: "loading"; text: string } | null;

/**
 * The one thing a filter row shows, in the inspector's own order: what the filter needs, an extension that is missing or
 * failed, the engine's error (codes stripped), then an extension that is switched off (a warning), then a model that is loading.
 */
export function rowStatus(i: { needs: string | null; ext: { failed: boolean; text: string } | null; error: string | undefined; loading: string | undefined }): RowStatus {
  if (i.needs) return { kind: "dot", tone: "error", text: i.needs };
  if (i.ext?.failed) return { kind: "dot", tone: "error", text: i.ext.text };
  if (i.error) return { kind: "dot", tone: "error", text: stripCodes(i.error) };
  if (i.ext) return { kind: "dot", tone: "warn", text: i.ext.text };
  if (i.loading) return { kind: "loading", text: i.loading };
  return null;
}
