/**
 * Where the Compare bar is and which way it lies, kept for the next time Compare is turned on (and the next launch: the page's own
 * `localStorage`, not the settings file: it is a viewing habit, not a camera setting). Whether Compare is on is the store's
 * `previewPoint === "compare"`, so a tool, Escape or the preview going off end it the way they end any point.
 */
import { create } from "zustand";
import { DEFAULT_SPLIT, clampPos, readSaved, swap, type Orientation, type Split } from "./compare";

const KEY = "better-cam.compare";

function load(): Split {
  try {
    return readSaved(globalThis.localStorage?.getItem(KEY) ?? null);
  } catch {
    return DEFAULT_SPLIT;
  }
}

function save(split: Split) {
  try {
    globalThis.localStorage?.setItem(KEY, JSON.stringify(split));
  } catch {
    // a full or blocked storage only loses the habit
  }
}

interface CompareState extends Split {
  setPos: (pos: number) => void;
  setOrientation: (orientation: Orientation) => void;
  swapOrientation: () => void;
}

export const useCompare = create<CompareState>((set, get) => {
  const apply = (patch: Partial<Split>) => {
    set(patch);
    const { pos, orientation } = get();
    save({ pos, orientation });
  };
  return {
    ...load(),
    setPos: (pos) => {
      const next = clampPos(pos);
      if (next !== get().pos) apply({ pos: next });
    },
    setOrientation: (orientation) => {
      if (orientation !== get().orientation) apply({ orientation });
    },
    swapOrientation: () => apply({ orientation: swap(get().orientation) }),
  };
});
