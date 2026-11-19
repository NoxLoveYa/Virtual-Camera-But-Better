/**
 * The preview tools (UX-PASS L2, L3): the eyedropper on a Chroma Key or Color Key card and the crop frame on a Crop/Pad card.
 * One tool runs at a time, for one card; it shows the picture entering that card (the preview point, L1) and ends with it.
 * Pure: no DOM, no store.
 */
import type { EngineStateName } from "../api";

export type PreviewToolKind = "pick" | "crop";

/** The running tool: which kind, for which card, and when it began (the settle window of `sourceChange` counts from there). */
export interface PreviewTool {
  kind: PreviewToolKind;
  filter: string;
  since: number;
}

/** The kind of tool a filter has a button for, if any. */
export function toolOf(kind: string): PreviewToolKind | null {
  if (kind === "chroma_key_filter" || kind === "color_key_filter") return "pick";
  if (kind === "crop_filter") return "crop";
  return null;
}

/** Why the tool buttons are disabled, as a tooltip; `null` when a tool can start (the preview is on and shows the live camera). */
export function toolBlock(s: { previewOn: boolean; state: EngineStateName; paused: boolean }): string | null {
  if (!s.previewOn) return "Turn the preview on to use this.";
  if (s.state !== "live") return "Start the camera to use this: it works on the live picture.";
  if (s.paused) return "The camera is paused: resume it to use this.";
  return null;
}

/** A frame's picture sizes get a moment to settle after a tool starts: frames of the old point may still arrive. */
export const SETTLE_MS = 600;

export interface SourceSize {
  w: number;
  h: number;
}

/**
 * Watches the source size of the frames while a tool runs. `next` is the baseline to keep and `changed` says the picture changed size
 * for good (the camera was reopened, an earlier filter changed the size): the tool ends. Within `SETTLE_MS` of the start a different
 * size only moves the baseline, since the first frames may still be the output's.
 */
export function sourceChange(baseline: SourceSize | null, size: SourceSize, since: number, now: number): { next: SourceSize; changed: boolean } {
  if (baseline === null || now - since < SETTLE_MS) return { next: size, changed: false };
  return { next: baseline, changed: baseline.w !== size.w || baseline.h !== size.h };
}
