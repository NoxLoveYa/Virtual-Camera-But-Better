import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent, type RefObject } from "react";
import { type Snapshot, useStore } from "../store";
import { lastFrame } from "../ui/lastFrame";
import {
  changedKeys,
  cursorOf,
  edgesOf,
  handleHalf,
  handlePoints,
  hitTest,
  nudgeOf,
  pan,
  paramsOf,
  readCrop,
  resize,
  sameEdges,
  shown,
  wheelFactor,
  zoom,
  type CropParams,
  type Edges,
  type Handle,
  type Size,
} from "../ui/cropRect";
import { letterbox, toSourceEdge } from "../ui/previewFrame";

interface Props {
  boxRef: RefObject<HTMLDivElement | null>;
  /** The Crop/Pad card this frame edits. */
  filter: string;
}

const HANDLES: Handle[] = ["nw", "n", "ne", "e", "se", "s", "sw", "w"];
const SIDES = ["l", "t", "r", "b"] as const;
/** A wheel or arrow-key burst is one gesture (one Undo) until it has been still this long. */
const BURST_MS = 450;
const HANDLE_HIT = 12;

/** A pointer drag under way: what was grabbed, the edges and params at the start (for the deltas and for Escape) and the chain to Undo to. */
interface Drag {
  grab: Handle | "inside";
  start: { x: number; y: number };
  edges: Edges;
  params: CropParams;
  before: Snapshot;
  changed: boolean;
}

const sizeText = (e: Edges) => `${e.r - e.l}×${e.b - e.t}`;

/**
 * Crop, zoom and pan on the picture (L3): the preview shows the picture entering the Crop/Pad card with a frame for the window and 8
 * handles. Drag a handle to move that edge (Shift keeps the aspect), drag inside to pan, wheel to zoom about the pointer, arrow keys to
 * nudge (Shift: 10). It writes the card's own params through `setParams`; every drag, wheel burst or nudge burst is one Undo. Escape
 * during a drag puts it back; Escape otherwise ends the tool.
 */
export function CropOverlay({ boxRef, filter }: Props) {
  useSyncExternalStore(lastFrame.subscribe, lastFrame.key);
  const params = useStore((s) => s.chain.find((f) => f.id === filter)?.params);
  const { setParams, checkpoint, sayUndo, endPreviewTool } = useStore.getState();
  const root = useRef<HTMLDivElement>(null);
  const [box, setBox] = useState<Size | null>(null);
  const drag = useRef<Drag | null>(null);
  const burst = useRef<{ before: Snapshot; timer: number } | null>(null);

  useLayoutEffect(() => {
    const el = boxRef.current;
    if (!el) return;
    const measure = () => setBox((p) => (p && p.w === el.clientWidth && p.h === el.clientHeight ? p : { w: el.clientWidth, h: el.clientHeight }));
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [boxRef]);

  const frame = lastFrame.get();
  const src: Size | null = frame ? { w: frame.header.sourceWidth, h: frame.header.sourceHeight } : null;
  const canvas = frame ? { width: frame.header.width, height: frame.header.height } : null;

  // ---- reading and writing the card
  const current = () => {
    const f = useStore.getState().chain.find((c) => c.id === filter);
    return f ? readCrop(f.params) : null;
  };
  /** Writes the keys of `next` that differ from the card now. Returns whether anything changed. */
  const write = (next: Edges): boolean => {
    const now = current();
    const size = lastFrame.get()?.header;
    if (!now || !size) return false;
    const s = { w: size.sourceWidth, h: size.sourceHeight };
    if (sameEdges(next, edgesOf(now, s))) return false;
    const patch = changedKeys(now, paramsOf(next, now.relative, s));
    if (Object.keys(patch).length === 0) return false;
    setParams(filter, patch);
    return true;
  };
  const say = (before: Snapshot) => {
    const now = current();
    const size = lastFrame.get()?.header;
    if (!now || !size) return;
    const { rect } = shown(edgesOf(now, { w: size.sourceWidth, h: size.sourceHeight }), { w: size.sourceWidth, h: size.sourceHeight });
    sayUndo(`Crop window ${sizeText(rect)} at ${rect.l}, ${rect.t}`, before);
  };

  /** Ends a wheel or arrow-key burst: one toast with one Undo for all of it. */
  const flushBurst = () => {
    const b = burst.current;
    if (!b) return;
    burst.current = null;
    window.clearTimeout(b.timer);
    say(b.before);
  };
  /** A change from the wheel or the keys: the first one of a burst takes the chain to Undo to, and the burst ends when it has been still. */
  const inBurst = (make: (truth: Edges, s: Size) => Edges) => {
    const now = current();
    const size = lastFrame.get()?.header;
    if (!now || !size) return;
    const s = { w: size.sourceWidth, h: size.sourceHeight };
    const before = burst.current?.before ?? checkpoint();
    if (!write(make(edgesOf(now, s), s))) return;
    if (burst.current) window.clearTimeout(burst.current.timer);
    burst.current = { before, timer: window.setTimeout(flushBurst, BURST_MS) };
  };

  const finishDrag = () => {
    const d = drag.current;
    if (!d) return;
    drag.current = null;
    if (d.changed) say(d.before);
  };
  const cancelDrag = () => {
    const d = drag.current;
    if (!d) return false;
    drag.current = null;
    const p = d.params;
    setParams(filter, { left: p.left, top: p.top, right: p.right, bottom: p.bottom, cx: p.cx, cy: p.cy });
    return true;
  };

  // ---- the window as it is drawn, in CSS pixels of the box
  const geo = (() => {
    if (!box || !src || !canvas || !params) return null;
    const lb = letterbox({ width: box.w, height: box.h }, canvas);
    const kx = (lb.s * canvas.width) / src.w;
    const ky = (lb.s * canvas.height) / src.h;
    const { rect, padded } = shown(edgesOf(readCrop(params), src), src);
    const picture = { x: lb.ox, y: lb.oy, w: canvas.width * lb.s, h: canvas.height * lb.s };
    const css = { x: lb.ox + rect.l * kx, y: lb.oy + rect.t * ky, w: (rect.r - rect.l) * kx, h: (rect.b - rect.t) * ky };
    return { lb, picture, rect, padded, css, handles: handlePoints(css, picture, handleHalf(css, HANDLE_HIT)) };
  })();

  const local = (e: { clientX: number; clientY: number }) => {
    const r = root.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };
  const sourcePoint = (e: { clientX: number; clientY: number }) => {
    const f = lastFrame.get();
    if (!f || !boxRef.current) return null;
    return toSourceEdge(e.clientX, e.clientY, boxRef.current.getBoundingClientRect(), f.header, { width: f.header.sourceWidth, height: f.header.sourceHeight });
  };

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || !geo || !src) return;
    root.current?.focus({ preventScroll: true });
    const p = local(e);
    const grab = hitTest(p.x, p.y, geo.css, HANDLE_HIT, geo.picture);
    const at = sourcePoint(e);
    const now = current();
    if (!grab || !at || !now) return;
    flushBurst();
    e.preventDefault();
    root.current?.setPointerCapture(e.pointerId);
    drag.current = { grab, start: at, edges: edgesOf(now, src), params: now, before: checkpoint(), changed: false };
  };
  const onPointerMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) {
      if (geo && root.current) {
        const p = local(e);
        root.current.style.cursor = cursorOf(hitTest(p.x, p.y, geo.css, HANDLE_HIT, geo.picture));
      }
      return;
    }
    const at = sourcePoint(e);
    const size = lastFrame.get()?.header;
    if (!at || !size) return;
    const s = { w: size.sourceWidth, h: size.sourceHeight };
    const dx = at.x - d.start.x;
    const dy = at.y - d.start.y;
    const next = d.grab === "inside" ? pan(d.edges, dx, dy, s) : resize(d.edges, d.grab, dx, dy, s, e.shiftKey);
    if (write(next)) d.changed = true;
  };
  const onPointerEnd = () => finishDrag();

  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    const n = nudgeOf(e.key, e.shiftKey);
    if (!n) return;
    e.preventDefault();
    if (drag.current) return;
    inBurst((truth, s) => pan(truth, n.dx, n.dy, s));
  };

  // Escape: puts a drag back, or else ends the tool. The wheel needs a listener that may cancel the page scroll, which React's does not allow.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      if (!cancelDrag()) endPreviewTool();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter]);
  useEffect(() => {
    const el = root.current!;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      if (drag.current) return;
      const at = sourcePoint(e);
      if (!at) return;
      const factor = wheelFactor(e.deltaY, e.deltaMode);
      inBurst((truth, s) => zoom(truth, factor, at.x, at.y, s));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter]);
  // The tool ends (Done, Escape, the picture changed size, the card went): a drag under way is put back, a burst is told.
  useEffect(() => {
    root.current?.focus({ preventScroll: true });
    return () => {
      cancelDrag();
      if (current()) flushBurst();
      else if (burst.current) window.clearTimeout(burst.current.timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filter]);

  const label = "Crop frame. Drag a handle to move an edge, drag inside to pan, scroll to zoom, arrow keys nudge by 1 pixel (Shift: 10), Escape to finish.";
  const padded = geo ? SIDES.filter((k) => geo.padded[k]) : [];
  return (
    <div
      ref={root}
      className="crop-overlay"
      data-tool="crop"
      tabIndex={0}
      role="group"
      aria-label={label}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerEnd}
      onPointerCancel={onPointerEnd}
      onLostPointerCapture={onPointerEnd}
      onKeyDown={onKeyDown}
    >
      {geo && (
        <>
          <div className="crop-picture" style={{ left: geo.picture.x, top: geo.picture.y, width: geo.picture.w, height: geo.picture.h }} />
          <div
            className={`crop-window${padded.map((k) => ` pad-${k}`).join("")}`}
            style={{ left: geo.css.x, top: geo.css.y, width: geo.css.w, height: geo.css.h }}
          />
          {HANDLES.map((h) => (
            <i
              key={h}
              className={`crop-handle${h.split("").some((c) => (c === "w" ? geo.padded.l : c === "e" ? geo.padded.r : c === "n" ? geo.padded.t : geo.padded.b)) ? " padded" : ""}`}
              data-handle={h}
              style={{ left: geo.handles[h].x, top: geo.handles[h].y }}
            />
          ))}
          <span className="crop-readout" role="status">
            {sizeText(geo.rect)} at {geo.rect.l}, {geo.rect.t}
            {padded.length > 0 && " · padded"}
          </span>
        </>
      )}
    </div>
  );
}
