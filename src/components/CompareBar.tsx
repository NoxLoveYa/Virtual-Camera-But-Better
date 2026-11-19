import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent, type RefObject } from "react";
import { DEFAULT_SPLIT, ORIENT_KEY, labelShown, pictureRect, posFromKey, posFromPointer } from "../ui/compare";
import { useCompare } from "../ui/compareState";

/**
 * The split-wipe Compare over the picture: a bar with a grip, "Before" (the camera, before every filter) on one side and "After"
 * (what apps get) on the other. The page draws the two pictures (`Preview.tsx`); this layer only moves the bar: drag the grip, or
 * press and drag anywhere on the picture to put the bar there. With the bar focused: arrow keys move it 2 % (Shift 10 %), Home and
 * End go to the edges. A double click puts it in the middle.
 */
export function CompareSplit({ boxRef, picture }: { boxRef: RefObject<HTMLDivElement | null>; picture: { w: number; h: number } | null }) {
  const pos = useCompare((s) => s.pos);
  const orientation = useCompare((s) => s.orientation);
  const { setPos } = useCompare.getState();
  const vertical = orientation === "vertical";

  // The picture's place inside the box (the canvas is letterboxed): the bar lives on the picture.
  const [box, setBox] = useState({ width: 0, height: 0 });
  useEffect(() => {
    const el = boxRef.current;
    if (!el) return;
    const measure = () => setBox((p) => (p.width === el.clientWidth && p.height === el.clientHeight ? p : { width: el.clientWidth, height: el.clientHeight }));
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [boxRef]);
  const rect = pictureRect(box, picture ? { width: picture.w, height: picture.h } : box);

  const layerRef = useRef<HTMLDivElement>(null);
  const gripRef = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);
  const moveTo = (e: ReactPointerEvent) => {
    const r = layerRef.current?.getBoundingClientRect();
    if (r) setPos(posFromPointer(e.clientX, e.clientY, { left: r.left, top: r.top, width: r.width, height: r.height }, orientation));
  };

  const at = `${pos * 100}%`;
  const extent = vertical ? rect.width : rect.height;
  return (
    <div
      ref={layerRef}
      className={`compare-layer ${vertical ? "is-vertical" : "is-horizontal"}`}
      style={{ left: rect.left, top: rect.top, width: rect.width, height: rect.height }}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault(); // no text selection, and the focus goes to the grip below
        e.currentTarget.setPointerCapture(e.pointerId);
        dragging.current = true;
        gripRef.current?.focus({ preventScroll: true });
        moveTo(e);
      }}
      onPointerMove={(e) => {
        if (dragging.current) moveTo(e);
      }}
      onPointerUp={() => (dragging.current = false)}
      onPointerCancel={() => (dragging.current = false)}
      onLostPointerCapture={() => (dragging.current = false)}
      onDoubleClick={() => setPos(DEFAULT_SPLIT.pos)}
    >
      <div
        ref={gripRef}
        className="compare-bar"
        style={vertical ? { left: at } : { top: at }}
        role="slider"
        tabIndex={0}
        aria-label="Compare: Before and After"
        aria-orientation={vertical ? "horizontal" : "vertical"}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(pos * 100)}
        aria-valuetext={`${Math.round(pos * 100)} % Before`}
        data-orientation={orientation}
        onKeyDown={(e: ReactKeyboardEvent) => {
          if (e.ctrlKey || e.metaKey || e.altKey) return;
          const next = posFromKey(pos, e.key, e.shiftKey);
          if (next === null) return;
          e.preventDefault();
          setPos(next);
        }}
      >
        <span className="compare-grip" aria-hidden />
      </div>
      {labelShown("before", pos, extent) && (
        <span className="compare-label is-before" style={vertical ? undefined : { top: `calc(${at} - 26px)` }}>
          Before
        </span>
      )}
      {labelShown("after", pos, extent) && (
        <span className="compare-label is-after" style={vertical ? undefined : { top: `calc(${at} + 6px)` }}>
          After
        </span>
      )}
    </div>
  );
}

/** The chip that turns the bar a quarter turn: left and right, or up and down. */
export function CompareOrient() {
  const orientation = useCompare((s) => s.orientation);
  const vertical = orientation === "vertical";
  return (
    <button
      type="button"
      className="preview-compare compare-orient"
      data-orientation={orientation}
      aria-label={vertical ? "Compare bar: left and right. Switch to up and down" : "Compare bar: up and down. Switch to left and right"}
      title={`The bar goes ${vertical ? "left and right" : "up and down"}: click to make it go ${vertical ? "up and down" : "left and right"} (${ORIENT_KEY})`}
      onClick={() => useCompare.getState().swapOrientation()}
    >
      <svg width="14" height="14" viewBox="0 0 14 14" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
        {vertical ? (
          <>
            <path d="M7 1.5v11" />
            <path d="M3.5 5 1.5 7l2 2M10.5 5l2 2-2 2" />
          </>
        ) : (
          <>
            <path d="M1.5 7h11" />
            <path d="M5 3.5 7 1.5l2 2M5 10.5l2 2 2-2" />
          </>
        )}
      </svg>
    </button>
  );
}
