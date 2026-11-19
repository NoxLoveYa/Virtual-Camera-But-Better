import { useRef, useState, type KeyboardEvent, type PointerEvent, type RefObject } from "react";
import { useStore } from "../store";
import { effectiveWidths, widthAfterKey, widthFromDrag, widthRange, type Side } from "../ui/layout";

interface Props {
  side: Side;
  /** The workspace's width in px (the window's). */
  total: number;
  workspaceRef: RefObject<HTMLDivElement | null>;
}

const VAR: Record<Side, string> = { nav: "--nav", inspector: "--insp" };
const LABEL: Record<Side, string> = { nav: "Resize navigator", inspector: "Resize inspector" };

/**
 * The drag handle on a hairline between two regions: `data-side="nav"` on the navigator's right edge, `data-side="inspector"`
 * on the inspector's left edge. The region's width comes from the CSS variable `--nav` or `--insp` on `.workspace`; a drag
 * writes it straight to that element (one layout per move, no React render of the app), and the width is saved once, on
 * release, in px. Double-click and Enter go back to auto (`null`).
 */
export function Splitter({ side, total, workspaceRef }: Props) {
  const navSaved = useStore((s) => s.ui.navWidth);
  const inspectorSaved = useStore((s) => s.ui.inspectorWidth);
  const navRail = useStore((s) => s.ui.navRail);
  const { setUi } = useStore.getState();
  // The width while a drag runs; null otherwise.
  const [live, setLive] = useState<number | null>(null);
  const drag = useRef<{ x: number; start: number; range: { min: number; max: number }; width: number } | null>(null);

  const widths = effectiveWidths(total, navSaved, inspectorSaved, navRail);
  const own = side === "nav" ? widths.nav : widths.inspector;
  const range = widthRange(side, total, side === "nav" ? widths.inspector : widths.nav);
  const width = live ?? own;
  const key = side === "nav" ? "navWidth" : "inspectorWidth";

  const apply = (px: number) => workspaceRef.current?.style.setProperty(VAR[side], `${px}px`);
  const commit = (px: number | null) => setUi({ [key]: px });

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    drag.current = { x: e.clientX, start: width, range, width };
    e.currentTarget.setPointerCapture(e.pointerId);
    document.body.classList.add("col-resizing");
    e.preventDefault();
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    d.width = widthFromDrag(side, d.start, e.clientX - d.x, d.range);
    apply(d.width);
    setLive(d.width);
  };
  const end = (save: boolean) => {
    const d = drag.current;
    if (!d) return;
    drag.current = null;
    document.body.classList.remove("col-resizing");
    setLive(null);
    // A click that moved nothing does not turn an auto width into a saved one.
    if (save && d.width !== d.start) commit(d.width);
    else apply(own);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape" && drag.current) {
      end(false);
      return;
    }
    const next = widthAfterKey(side, e.key, e.shiftKey, width, range);
    if (next === undefined) return;
    e.preventDefault();
    commit(next);
  };

  return (
    <div
      className={`splitter${live !== null ? " dragging" : ""}`}
      data-side={side}
      role="separator"
      aria-orientation="vertical"
      aria-label={LABEL[side]}
      tabIndex={0}
      aria-valuemin={range.min}
      aria-valuemax={range.max}
      aria-valuenow={width}
      aria-valuetext={`${width} px`}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={() => end(true)}
      onPointerCancel={() => end(false)}
      onLostPointerCapture={() => end(true)}
      onDoubleClick={() => commit(null)}
      onKeyDown={onKeyDown}
    />
  );
}
