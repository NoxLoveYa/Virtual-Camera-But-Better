/**
 * The last preview frame, for the tools that read it (the eyedropper samples its NV12 bytes; the crop frame maps pointer positions with its
 * sizes). `Preview` puts each frame here; a frame is the engine's buffer as it came, kept by reference, no copy. `subscribe` calls
 * back only when the picture's geometry (canvas size or source size) changes, not on every frame.
 */
import type { FrameHeader } from "./previewFrame";

export interface LastFrame {
  buf: ArrayBuffer;
  header: FrameHeader;
}

let last: LastFrame | null = null;
const listeners = new Set<() => void>();

const geometry = (h: FrameHeader | undefined) => (h ? `${h.width}x${h.height}/${h.sourceWidth}x${h.sourceHeight}` : "");

export const lastFrame = {
  get: (): LastFrame | null => last,
  set(buf: ArrayBuffer, header: FrameHeader) {
    const changed = geometry(last?.header) !== geometry(header);
    last = { buf, header };
    if (changed) listeners.forEach((l) => l());
  },
  clear() {
    if (last === null) return;
    last = null;
    listeners.forEach((l) => l());
  },
  subscribe(listener: () => void) {
    listeners.add(listener);
    return () => void listeners.delete(listener);
  },
  /** A stable snapshot of the geometry for `useSyncExternalStore`: a string, so equal geometry is equal value. */
  key: () => geometry(last?.header),
};
