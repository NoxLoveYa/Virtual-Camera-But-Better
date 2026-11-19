import { useStore } from "../store";
import { effectiveSource, findCamera, findStored } from "../camera/schema";
import { cameraSummary, controlsSummary, outputSummary } from "./navSummary";

/** The three sections the navigator lists and the inspector can show. */
export type SectionId = "camera" | "controls" | "output";

/**
 * The one-line summary of a section, as text from the store only (never a read of the camera, which would light its LED). The
 * navigator row and the inspector header say the same line.
 */
export function useSummary(id: SectionId): string {
  return useStore((s) => {
    if (id === "output") return outputSummary(s.output);
    const cam = s.cameras ? findCamera(s.cameras, s.source.video_device_id) : null;
    if (id === "camera") return cameraSummary(effectiveSource(s.source, s.inUse), s.cameras, cam);
    return controlsSummary(cam, cam ? (findStored(s.controls, cam)?.device ?? null) : null);
  });
}
