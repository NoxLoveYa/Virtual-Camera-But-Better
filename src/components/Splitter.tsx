import { useEffect, useRef, useState, type KeyboardEvent, type PointerEvent, type RefObject } from "react";
import { useStore } from "../store";
import { DEFAULT_SPLIT, clampSplit, splitAfterKey, splitBounds, splitFromWidth } from "../ui/layout";

/**
 * The drag handle on the line between the two columns (hidden when they stack). The columns' width comes from the CSS
 * variable `--split` on `.cols`; a drag writes it straight to that element (one layout per move, no React render of the
 * app), and the ratio is saved once, on release. Double-click and Enter go back to the default.
 */
export function Splitter({ colsRef }: { colsRef: RefObject<HTMLDivElement | null> }) {
  const saved = useStore((s) => s.ui.split);
  const { setUi } = useStore.getState();
  const [total, setTotal] = useState(0);
  // The ratio while a drag runs; null otherwise.
  const [live, setLive] = useState<number | null>(null);
  const drag = useRef<{ x: number; left: number; total: number; ratio: number } | null>(null);

  useEffect(() => {
    const el = colsRef.current;
    if (!el) return;
    const read = () => setTotal(el.clientWidth);
    read();
    const ro = new ResizeObserver(read);
    ro.observe(el);
    return () => ro.disconnect();
  }, [colsRef]);

  const ratio = clampSplit(live ?? saved, total);
  const { min, max } = splitBounds(total);

  const apply = (r: number) => colsRef.current?.style.setProperty("--split", String(r));
  const commit = (r: number) => {
    apply(r);
    setUi({ split: r });
  };

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    const cols = colsRef.current;
    if (e.button !== 0 || !cols) return;
    const left = cols.querySelector<HTMLElement>(".col-left")!.getBoundingClientRect().width;
    drag.current = { x: e.clientX, left, total: cols.clientWidth, ratio };
    e.currentTarget.setPointerCapture(e.pointerId);
    document.body.classList.add("col-resizing");
    e.preventDefault();
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    d.ratio = splitFromWidth(d.left + e.clientX - d.x, d.total);
    apply(d.ratio);
    setLive(d.ratio);
  };
  const end = (save: boolean) => {
    const d = drag.current;
    if (!d) return;
    drag.current = null;
    document.body.classList.remove("col-resizing");
    setLive(null);
    if (save) commit(d.ratio);
    else apply(saved);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape" && drag.current) {
      end(false);
      return;
    }
    const next = splitAfterKey(e.key, e.shiftKey, ratio, total);
    if (next === null) return;
    e.preventDefault();
    commit(next);
  };

  return (
    <div
      className={`splitter${live !== null ? " dragging" : ""}`}
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize columns"
      tabIndex={0}
      aria-valuemin={Math.round(min * 100)}
      aria-valuemax={Math.round(max * 100)}
      aria-valuenow={Math.round(ratio * 100)}
      aria-valuetext={`${Math.round(ratio * 100)}% for the left column`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={() => end(true)}
      onPointerCancel={() => end(false)}
      onLostPointerCapture={() => end(true)}
      onDoubleClick={() => commit(clampSplit(DEFAULT_SPLIT, total))}
      onKeyDown={onKeyDown}
    />
  );
}
