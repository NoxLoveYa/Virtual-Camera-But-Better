/**
 * Reorder of the filter chain by pointer and by keyboard (W1). The pure rules (which gap the pointer is in, how fast the
 * column scrolls, what the arrow keys do) are in `chainDrag.ts`; this file wires them to pointer events, the scrolling
 * column and the store.
 *
 * Nothing is written to the store while a card is being dragged or lifted: only the state that draws the drop line, the
 * lifted copy and the faded slot changes. The order is committed once, on drop, through `moveFilter` (one `set_chain`,
 * one Undo step). Cancelling never touches the store.
 */
import { useLayoutEffect, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from "react";
import { useStore } from "../store";
import { labelOf } from "../filters/schema";
import { DRAG_THRESHOLD, edgeSpeed, finalIndex, insertionIndex, keyboardGap, snapToSource, type StepKey } from "./chainDrag";

export interface DragView {
  id: string;
  from: number;
  /** The gap the card would be dropped into (`from` when dropping changes nothing). */
  k: number;
  mode: "pointer" | "keyboard";
}

/** The copy of the card's header that follows the pointer. */
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
  /** Latest pointer Y in client pixels. */
  y: number;
  /** Distance from the card's top edge to the pointer at the press: the lifted copy keeps it. */
  grabY: number;
  left: number;
  width: number;
  cleanups: Array<() => void>;
}

// ---------------------------------------------------------------------------------------------------------------------
// Focus: a moved card keeps the focus on the control the user was on.

interface FocusSpot {
  id: string;
  index: number;
  act?: string;
}

const FOCUSABLE = "button, input, select, textarea, a[href], [tabindex]";

const cardOf = (id: string) => document.querySelector<HTMLElement>(`[data-filter-id="${CSS.escape(id)}"]`);

/** The card's own controls, without the "+" of the gap above it (the first card has none, so counting it would shift the index when a card moves to or from the top). */
const focusablesOf = (card: HTMLElement) => Array.from(card.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => !el.closest(".gap-add"));

function captureFocus(): FocusSpot | null {
  const el = document.activeElement as HTMLElement | null;
  const card = el?.closest<HTMLElement>("[data-filter-id]");
  if (!el || !card?.dataset.filterId || el.closest(".gap-add")) return null;
  return { id: card.dataset.filterId, index: focusablesOf(card).indexOf(el), act: el.dataset.act };
}

/** After the next frame, focuses the same control of the same card. A "Move up" that became disabled hands over to "Move down" (and the reverse). */
function restoreFocus(spot: FocusSpot | null) {
  if (!spot) return;
  requestAnimationFrame(() => {
    const card = cardOf(spot.id);
    if (!card) return;
    let target: HTMLElement | null = focusablesOf(card)[spot.index] ?? null;
    if (!target || (target as HTMLButtonElement).disabled) {
      const other = spot.act === "up" ? "down" : spot.act === "down" ? "up" : "grip";
      target = card.querySelector<HTMLElement>(`[data-act="${other}"]:not(:disabled)`) ?? card.querySelector<HTMLElement>('[data-act="grip"]');
    }
    target?.focus();
  });
}

/** Moves a card (with its Undo toast) and keeps the focus on the control that was used. */
export function moveWithFocus(from: number, to: number) {
  const spot = captureFocus();
  useStore.getState().moveFilter(from, to);
  restoreFocus(spot);
}

/** The nearest ancestor that scrolls (`.col-right` in two columns, `.cols` when stacked: there `.col` is `display: contents`). */
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

    /** Rects of the cards and of the two terminals, to place the line and to find the gap under the pointer. */
    const measure = () => {
      const ol = chainRef.current;
      const block = blockRef.current;
      if (!ol || !block) return null;
      const items = Array.from(ol.children) as HTMLElement[];
      const cards = items.filter((li) => li.classList.contains("step")).map((li) => (li.querySelector(".card") ?? li).getBoundingClientRect());
      if (cards.length === 0) return null;
      return { cards, first: items[0].getBoundingClientRect(), last: items[items.length - 1].getBoundingClientRect(), block: block.getBoundingClientRect() };
    };
    type Measure = NonNullable<ReturnType<typeof measure>>;

    /** The line goes in the middle of the gap above card k (under the last card for k = n). */
    const gapTop = (m: Measure, k: number) => {
      const above = k === 0 ? m.first.bottom : m.cards[k - 1].bottom;
      const below = k === m.cards.length ? m.last.top : m.cards[k].top;
      return Math.round((above + below) / 2 - m.block.top - 1);
    };

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
      const card = target.closest(".card")?.getBoundingClientRect();
      if (from < 0 || chain.length < 2 || !card) return;

      const s: Session = {
        id,
        from,
        k: from,
        mode: "pointer",
        started: false,
        pointerId: e.pointerId,
        startX: e.clientX,
        startY: e.clientY,
        y: e.clientY,
        grabY: e.clientY - card.top,
        left: card.left,
        width: card.width,
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
      const s: Session = { id, from, k: from, mode: "keyboard", started: true, pointerId: 0, startX: 0, startY: 0, y: 0, grabY: 0, left: 0, width: 0, cleanups: [] };
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

    /** Alt+ArrowUp / Alt+ArrowDown with the focus anywhere inside a card moves it one step. */
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

    return { moveLift, onPointerDown, onGripKeyDown, onGripBlur, onCardKeyDown, cancel: () => finish(false) };
  }, []);

  // The lifted copy is mounted after the first move: put it at the pointer before it is painted.
  useLayoutEffect(() => {
    if (lift) api.moveLift();
  }, [lift, api]);

  // Leaving the page in the middle of a drag releases everything it holds.
  useEffect(() => api.cancel, [api]);

  // `handlers` is one object for the life of the component, so a memoised card does not re-render for it.
  return { drag, lineTop, lift, announcement, chainRef, blockRef, liftRef, handlers: api };
}

/** The pointer and key handlers of a card; the same object on every render. */
export type ChainHandlers = ReturnType<typeof useChainDrag>["handlers"];
