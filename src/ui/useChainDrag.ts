/**
 * Reorder of the filter chain by pointer and by keyboard (W1). The pure rules (which gap the pointer is in, how fast the
 * column scrolls, what the arrow keys do) are in `chainDrag.ts`, the row geometry in `chainGeometry.ts`; this file wires
 * them to pointer events, the scrolling navigator and the store.
 *
 * The rows are the navigator's (`li.nav-step[data-filter-id]`). A drag or a lift can start on a row or on the inspector's
 * grip: either way the drop line is drawn in the navigator list.
 *
 * Nothing is written to the store while a filter is being dragged or lifted: only the state that draws the drop line, the
 * lifted copy and the faded slot changes. The order is committed once, on drop, through `moveFilter` (one `set_chain`,
 * one Undo step). Cancelling never touches the store.
 */
import { useLayoutEffect, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from "react";
import { useStore } from "../store";
import { labelOf } from "../filters/schema";
import { DRAG_THRESHOLD, edgeSpeed, finalIndex, insertionIndex, keyboardGap, snapToSource, type StepKey } from "./chainDrag";
import { gapLine, rowRects } from "./chainGeometry";

export interface DragView {
  id: string;
  from: number;
  /** The gap the card would be dropped into (`from` when dropping changes nothing). */
  k: number;
  mode: "pointer" | "keyboard";
}

/** The copy of the filter's row that follows the pointer. */
export interface LiftView {
  id: string;
  label: string;
  bypassed: boolean;
  left: number;
  width: number;
}

interface Session {
  id: string;
  from: number;
  k: number;
  mode: "pointer" | "keyboard";
  started: boolean;
  pointerId: number;
  startX: number;
  startY: number;
  /** Latest pointer position in client pixels. */
  x: number;
  y: number;
  /** Distance from the row's top edge to the pointer at the press: the lifted copy keeps it. */
  grabY: number;
  /** A drag that started outside the navigator (the inspector's grip): the lifted copy follows the pointer sideways too, with this offset. */
  grabX: number | null;
  left: number;
  width: number;
  cleanups: Array<() => void>;
}

// ---------------------------------------------------------------------------------------------------------------------
// Focus: a moved row keeps the focus on the control the user was on. Only a focus in a navigator row is put back: a move
// does not touch the inspector's DOM, so its focus survives (the grip, a range input). The one exception is the
// inspector header's Move up / Move down, which can disable themselves at an end of the chain.

interface FocusSpot {
  id: string;
  index: number;
  act?: string;
  /** Where the focus was: a navigator row, or the inspector. */
  scope: "nav" | "inspector";
}

const FOCUSABLE = "button, input, select, textarea, a[href], [tabindex]";

/** The navigator row of a filter. */
const rowOf = (id: string) => document.querySelector<HTMLElement>(`li.nav-step[data-filter-id="${CSS.escape(id)}"]`);
const inspectorOf = (id: string) => document.querySelector<HTMLElement>(`[data-inspects="${CSS.escape(id)}"]`);

/** The row's own controls, without the "+" of the gap above it (the first row has none, so counting it would shift the index when a row moves to or from the top). */
const focusablesOf = (row: HTMLElement) => Array.from(row.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => !el.closest(".gap-add"));

function captureFocus(): FocusSpot | null {
  const el = document.activeElement as HTMLElement | null;
  if (!el || el.closest(".gap-add")) return null;
  const row = el.closest<HTMLElement>("li.nav-step[data-filter-id]");
  if (row?.dataset.filterId) return { id: row.dataset.filterId, index: focusablesOf(row).indexOf(el), act: el.dataset.act, scope: "nav" };
  const insp = el.closest<HTMLElement>("[data-inspects]");
  if (insp?.dataset.inspects && (el.dataset.act === "up" || el.dataset.act === "down")) {
    return { id: insp.dataset.inspects, index: -1, act: el.dataset.act, scope: "inspector" };
  }
  return null;
}

/** After the next frame, focuses the same control of the same row. A "Move up" that became disabled hands over to "Move down" (and the reverse). */
function restoreFocus(spot: FocusSpot | null) {
  if (!spot) return;
  requestAnimationFrame(() => {
    const root = spot.scope === "nav" ? rowOf(spot.id) : inspectorOf(spot.id);
    if (!root) return;
    let target: HTMLElement | null = spot.scope === "nav" ? (focusablesOf(root)[spot.index] ?? null) : root.querySelector<HTMLElement>(`[data-act="${spot.act}"]`);
    if (!target || (target as HTMLButtonElement).disabled) {
      const other = spot.act === "up" ? "down" : spot.act === "down" ? "up" : "grip";
      target = root.querySelector<HTMLElement>(`[data-act="${other}"]:not(:disabled)`) ?? root.querySelector<HTMLElement>('[data-act="grip"]');
    }
    target?.focus();
  });
}

/** Moves a filter (with its Undo toast) and keeps the focus on the control that was used. */
export function moveWithFocus(from: number, to: number) {
  const spot = captureFocus();
  useStore.getState().moveFilter(from, to);
  restoreFocus(spot);
}

/** The nearest ancestor that scrolls (`.nav-scroll`, the navigator's list). */
export function findScroller(from: HTMLElement | null): HTMLElement {
  for (let p = from?.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p).overflowY;
    if ((o === "auto" || o === "scroll") && p.clientHeight > 0) return p;
  }
  return (document.scrollingElement as HTMLElement) ?? document.documentElement;
}

const labelAt = (index: number) => {
  const f = useStore.getState().chain[index];
  return f ? labelOf(f.kind, useStore.getState().extensions) : "";
};

export function useChainDrag() {
  const [drag, setDrag] = useState<DragView | null>(null);
  /** The drop line's top, relative to `.chain-block`. */
  const [lineTop, setLineTop] = useState(0);
  const [lift, setLift] = useState<LiftView | null>(null);
  const [announcement, setAnnouncement] = useState("");
  const chainRef = useRef<HTMLOListElement>(null);
  const blockRef = useRef<HTMLDivElement>(null);
  const liftRef = useRef<HTMLDivElement>(null);
  const session = useRef<Session | null>(null);

  const api = useMemo(() => {
    const say = (text: string) => setAnnouncement(text);

    /** Rects of the rows, the list and the block, to place the line and to find the gap under the pointer. */
    const measure = () => {
      const ol = chainRef.current;
      const block = blockRef.current;
      if (!ol || !block) return null;
      const cards = rowRects(block);
      if (cards.length === 0) return null;
      return { cards, list: ol.getBoundingClientRect(), block: block.getBoundingClientRect() };
    };
    type Measure = NonNullable<ReturnType<typeof measure>>;

    /** The line goes in the middle of the gap above row k (under the last row for k = n). */
    const gapTop = (m: Measure, k: number) => gapLine(m.cards, k, m.list.top, m.list.bottom, m.block.top);

    const showGap = (k: number) => {
      const m = measure();
      if (m) setLineTop(gapTop(m, k));
    };

    const setGap = (s: Session, k: number) => {
      s.k = k;
      setDrag((d) => (d && d.k !== k ? { ...d, k } : d));
    };

    /** Recomputes the gap from the latest pointer position and the current scroll. */
    const updatePointer = () => {
      const s = session.current;
      if (!s || !s.started || s.mode !== "pointer") return;
      const m = measure();
      if (!m) return;
      const k = snapToSource(s.from, insertionIndex(m.cards.map((r) => r.top + r.height / 2), s.y));
      setGap(s, k);
      setLineTop(gapTop(m, k));
    };

    const moveLift = () => {
      const s = session.current;
      const el = liftRef.current;
      if (!s || !el) return;
      el.style.top = `${Math.max(0, Math.min(window.innerHeight - el.offsetHeight, s.y - s.grabY))}px`;
      if (s.grabX !== null) el.style.left = `${Math.max(0, Math.min(window.innerWidth - el.offsetWidth, s.x - s.grabX))}px`;
    };

    /** When the last pointer drag ended (0: none): the click that follows a drag must not select the row. */
    let dragEnd = 0;
    /** True once for the click that comes straight after a pointer drag ended on the element it started on. */
    const consumeDragClick = () => {
      const was = performance.now() - dragEnd < 100;
      dragEnd = 0;
      return was;
    };

    const commit = (s: Session, announce: boolean) => {
      const chain = useStore.getState().chain;
      if (chain[s.from]?.id !== s.id) return;
      const final = finalIndex(s.from, s.k);
      const label = labelAt(s.from);
      if (final !== s.from) {
        moveWithFocus(s.from, final);
        if (announce) say(`${label} moved to step ${final + 1}.`);
      } else if (announce) {
        say(`${label} stays at step ${s.from + 1}.`);
      }
    };

    /** Ends the drag or lift. A cancel leaves the chain, the toast and the Undo as they were. */
    const finish = (ok: boolean) => {
      const s = session.current;
      if (!s) return;
      if (ok && s.started && s.mode === "pointer") updatePointer();
      if (s.started && s.mode === "pointer") dragEnd = performance.now();
      session.current = null;
      s.cleanups.forEach((fn) => fn());
      setDrag(null);
      setLift(null);
      if (s.mode === "keyboard") {
        if (ok) commit(s, true);
        else say("Move cancelled.");
      } else if (ok && s.started) {
        commit(s, false);
      }
    };

    /** The drag proper: starts after the pointer moved a few pixels. */
    const activate = (s: Session, scroller: HTMLElement) => {
      s.started = true;
      const f = useStore.getState().chain[s.from];
      setDrag({ id: s.id, from: s.from, k: s.from, mode: "pointer" });
      setLift({ id: s.id, label: labelOf(f.kind, useStore.getState().extensions), bypassed: !f.enabled, left: s.left, width: s.width });
      document.body.classList.add("reordering");
      getSelection()?.removeAllRanges();

      const onScroll = () => updatePointer();
      scroller.addEventListener("scroll", onScroll, { passive: true });
      // Scrolls while the pointer rests in the edge zone, even without pointer events.
      let raf = 0;
      const tick = () => {
        const r = scroller.getBoundingClientRect();
        const v = edgeSpeed(s.y, r.top, r.bottom);
        if (v !== 0) scroller.scrollTop += Math.sign(v) * Math.max(1, Math.round(Math.abs(v)));
        raf = requestAnimationFrame(tick);
      };
      raf = requestAnimationFrame(tick);
      s.cleanups.push(() => {
        cancelAnimationFrame(raf);
        scroller.removeEventListener("scroll", onScroll);
        document.body.classList.remove("reordering");
      });
    };

    const onPointerDown = (e: ReactPointerEvent<HTMLElement>, id: string) => {
      if (session.current || e.button !== 0 || !e.isPrimary) return;
      const chain = useStore.getState().chain;
      const from = chain.findIndex((f) => f.id === id);
      const target = e.currentTarget;
      const row = rowOf(id)?.getBoundingClientRect();
      if (from < 0 || chain.length < 2 || !row) return;
      // On a navigator row the lifted copy stays over the row's column; from the inspector's grip the pointer is somewhere
      // else, so the copy hangs from the pointer (half a row down and a little to the right).
      const onRow = target.closest("li.nav-step") !== null;

      const s: Session = {
        id,
        from,
        k: from,
        mode: "pointer",
        started: false,
        pointerId: e.pointerId,
        startX: e.clientX,
        startY: e.clientY,
        x: e.clientX,
        y: e.clientY,
        grabY: onRow ? e.clientY - row.top : row.height / 2,
        grabX: onRow ? null : 16,
        left: onRow ? row.left : e.clientX - 16,
        width: row.width,
        cleanups: [],
      };
      session.current = s;
      try {
        target.setPointerCapture(e.pointerId);
      } catch {
        // The pointer is already gone: the up or cancel event will not come, so the press ends here.
        session.current = null;
        return;
      }

      const scroller = findScroller(chainRef.current);
      const move = (ev: PointerEvent) => {
        if (ev.pointerId !== s.pointerId) return;
        s.x = ev.clientX;
        s.y = ev.clientY;
        if (!s.started) {
          if (Math.hypot(ev.clientX - s.startX, ev.clientY - s.startY) < DRAG_THRESHOLD) return;
          activate(s, scroller);
        }
        moveLift();
        updatePointer();
      };
      const up = (ev: PointerEvent) => {
        if (ev.pointerId !== s.pointerId) return;
        s.y = ev.clientY;
        finish(true);
      };
      const abort = () => finish(false);
      const esc = (ev: KeyboardEvent) => {
        if (ev.key !== "Escape") return;
        ev.preventDefault();
        ev.stopPropagation();
        finish(false);
      };
      target.addEventListener("pointermove", move);
      target.addEventListener("pointerup", up);
      target.addEventListener("pointercancel", abort);
      target.addEventListener("lostpointercapture", abort);
      window.addEventListener("keydown", esc, true);
      window.addEventListener("blur", abort);
      // The listeners come off before the capture is released, so releasing it does not read as a cancel.
      s.cleanups.unshift(() => {
        target.removeEventListener("pointermove", move);
        target.removeEventListener("pointerup", up);
        target.removeEventListener("pointercancel", abort);
        target.removeEventListener("lostpointercapture", abort);
        window.removeEventListener("keydown", esc, true);
        window.removeEventListener("blur", abort);
        if (target.hasPointerCapture(s.pointerId)) target.releasePointerCapture(s.pointerId);
      });
    };

    // ---- keyboard ----

    /** Keeps the line inside the scrolling column while the arrow keys move it. */
    const revealLine = (k: number) => {
      const m = measure();
      if (!m) return;
      const scroller = findScroller(chainRef.current);
      const r = scroller.getBoundingClientRect();
      const y = m.block.top + gapTop(m, k);
      const pad = 48;
      if (y < r.top + pad) scroller.scrollTop -= r.top + pad - y;
      else if (y > r.bottom - pad) scroller.scrollTop += y - (r.bottom - pad);
    };

    const liftKeyboard = (id: string) => {
      const chain = useStore.getState().chain;
      const from = chain.findIndex((f) => f.id === id);
      if (session.current || from < 0 || chain.length < 2) return;
      const s: Session = { id, from, k: from, mode: "keyboard", started: true, pointerId: 0, startX: 0, startY: 0, x: 0, y: 0, grabY: 0, grabX: null, left: 0, width: 0, cleanups: [] };
      session.current = s;
      const abort = () => finish(false);
      window.addEventListener("blur", abort);
      s.cleanups.push(() => window.removeEventListener("blur", abort));
      setDrag({ id, from, k: from, mode: "keyboard" });
      showGap(from);
      say(`Lifted ${labelAt(from)}, step ${from + 1} of ${chain.length}. Up and down arrows move it, Space drops it, Escape cancels.`);
    };

    const onGripKeyDown = (e: ReactKeyboardEvent<HTMLElement>, id: string) => {
      if (e.altKey || e.ctrlKey || e.metaKey) return;
      const s = session.current;
      if (s && s.mode === "keyboard" && s.id === id) {
        switch (e.key) {
          case " ":
          case "Enter":
            e.preventDefault();
            finish(true);
            break;
          case "Escape":
            e.preventDefault();
            finish(false);
            break;
          case "Tab":
            finish(false);
            break;
          case "ArrowUp":
          case "ArrowDown":
          case "Home":
          case "End": {
            e.preventDefault();
            const n = useStore.getState().chain.length;
            const k = keyboardGap(s.from, s.k, e.key as StepKey, n);
            setGap(s, k);
            showGap(k);
            revealLine(k);
            say(`Step ${finalIndex(s.from, k) + 1} of ${n}.`);
            break;
          }
        }
        return;
      }
      if (!s && (e.key === " " || e.key === "Enter")) {
        e.preventDefault();
        liftKeyboard(id);
      }
    };

    const onGripBlur = (id: string) => {
      const s = session.current;
      if (s && s.mode === "keyboard" && s.id === id) finish(false);
    };

    /** Alt+ArrowUp / Alt+ArrowDown with the focus on the filter's navigator row or anywhere in its inspector moves it one step. */
    const onCardKeyDown = (e: ReactKeyboardEvent<HTMLElement>, id: string) => {
      if (!e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
      e.preventDefault();
      if (session.current) return;
      const chain = useStore.getState().chain;
      const from = chain.findIndex((f) => f.id === id);
      const to = e.key === "ArrowUp" ? from - 1 : from + 1;
      if (from < 0 || to < 0 || to >= chain.length) return;
      moveWithFocus(from, to);
      say(`${labelAt(to)} moved to step ${to + 1}.`);
    };

    return { moveLift, onPointerDown, onGripKeyDown, onGripBlur, onCardKeyDown, consumeDragClick, cancel: () => finish(false) };
  }, []);

  // The lifted copy is mounted after the first move: put it at the pointer before it is painted.
  useLayoutEffect(() => {
    if (lift) api.moveLift();
  }, [lift, api]);

  // Leaving the page in the middle of a drag releases everything it holds.
  useEffect(() => api.cancel, [api]);

  // `handlers` is one object for the life of the component, so a memoised row does not re-render for it.
  return { drag, lineTop, lift, announcement, chainRef, blockRef, liftRef, handlers: api };
}

/** The pointer and key handlers of a row or an inspector; the same object on every render. */
export type ChainHandlers = ReturnType<typeof useChainDrag>["handlers"];
