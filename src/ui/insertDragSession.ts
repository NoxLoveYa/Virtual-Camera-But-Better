/**
 * Dragging a filter out of the Add menu into the chain (L5). The press starts on a menu item; after a few pixels the menu
 * closes (the caller's `onActivate`) and a small chip follows the pointer while the chain shows the same drop line as the
 * reorder (W1). Release over the chain adds the filter at that gap, in one `set_chain` with one Undo toast (`addFilter`).
 * Escape, a lost pointer or leaving the window cancels and adds nothing.
 *
 * No HTML5 drag and drop (Tauri's file drops are on): pointer events on `window`, so the drag outlives the menu item
 * that was pressed. What a press turns into, and whether the pointer is over the chain, are the pure rules in
 * `insertDrag.ts`. The view (chip, line) is a tiny store that `App.tsx` draws.
 */
import type { PointerEvent as ReactPointerEvent } from "react";
import { create } from "zustand";
import { useStore } from "../store";
import { labelOf, type FilterKind } from "../filters/schema";
import { edgeSpeed, insertionIndex } from "./chainDrag";
import { classifyPress, overChain } from "./insertDrag";
import { findScroller } from "./useChainDrag";

/** What the page draws while a filter is dragged out of the menu. */
export interface InsertView {
  label: string;
  /** The pointer, in client pixels: the chip follows it. */
  x: number;
  y: number;
  /** Where the line goes in `.chain-block`; `null` while the pointer is not over the chain (a drop would add nothing). */
  lineTop: number | null;
}

export const useInsertView = create<{ view: InsertView | null }>(() => ({ view: null }));

interface Session {
  kind: FilterKind;
  label: string;
  pointerId: number;
  pointerType: string;
  startX: number;
  startY: number;
  x: number;
  y: number;
  started: boolean;
  /** The gap under the pointer, `null` outside the chain. */
  k: number | null;
  scroller: HTMLElement | null;
  cleanups: Array<() => void>;
}

let session: Session | null = null;

/** The gap of the chain under client `y` and where its line goes (relative to `.chain-block`); `null` without a chain block. */
function gapAt(block: HTMLElement, y: number): { k: number; lineTop: number } | null {
  const ol = block.querySelector<HTMLElement>(".chain");
  if (!ol) return null;
  const items = Array.from(ol.children) as HTMLElement[];
  if (items.length === 0) return null;
  const top = block.getBoundingClientRect().top;
  // An empty chain shows its hint between the terminals: the line goes at the top of that box, not through the text.
  const empty = ol.querySelector(".empty");
  if (empty && !items.some((li) => li.classList.contains("step"))) return { k: 0, lineTop: Math.round(empty.getBoundingClientRect().top - top - 1) };
  const cards = items.filter((li) => li.classList.contains("step")).map((li) => (li.querySelector(".card") ?? li).getBoundingClientRect());
  const k = insertionIndex(cards.map((r) => r.top + r.height / 2), y);
  const above = k === 0 ? items[0].getBoundingClientRect().bottom : cards[k - 1].bottom;
  const below = k === cards.length ? items[items.length - 1].getBoundingClientRect().top : cards[k].top;
  return { k, lineTop: Math.round((above + below) / 2 - top - 1) };
}

/** Works out the gap under the pointer (the chain may have scrolled since the last move) and publishes the view. */
function refresh(s: Session) {
  const block = document.querySelector<HTMLElement>('[data-drop="chain"]');
  const hit = block && s.scroller && overChain(s.x, s.y, block.getBoundingClientRect(), s.scroller.getBoundingClientRect()) ? gapAt(block, s.y) : null;
  s.k = hit?.k ?? null;
  useInsertView.setState({ view: { label: s.label, x: s.x, y: s.y, lineTop: hit?.lineTop ?? null } });
}

function activate(s: Session, onActivate: () => void) {
  s.started = true;
  s.scroller = findScroller(document.querySelector<HTMLElement>('[data-drop="chain"] .chain'));
  onActivate();
  document.body.classList.add("reordering");
  getSelection()?.removeAllRanges();
  const scroller = s.scroller;
  const onScroll = () => refresh(s);
  scroller.addEventListener("scroll", onScroll, { passive: true });
  // Scrolls while the pointer rests in the edge zone, even without pointer events (the same speed curve as the reorder).
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
}

/** Ends the drag. A cancel (or a release outside the chain) leaves the chain, the toast and the Undo as they were. */
function finish(ok: boolean) {
  const s = session;
  if (!s) return;
  if (ok && s.started) refresh(s);
  session = null;
  s.cleanups.forEach((fn) => fn());
  useInsertView.setState({ view: null });
  if (ok && s.started && s.k !== null) useStore.getState().addFilter(s.kind, s.k);
}

/**
 * Call from `onPointerDown` of an Add-menu item. Nothing happens for a plain click; once the pointer has moved, the drag
 * starts and `onActivate` runs (the caller closes the menu there, so the chain is not covered).
 */
export function beginInsertDrag(e: ReactPointerEvent<HTMLElement>, kind: FilterKind, onActivate: () => void) {
  if (session || e.button !== 0 || !e.isPrimary) return;
  const s: Session = {
    kind,
    label: labelOf(kind, useStore.getState().extensions),
    pointerId: e.pointerId,
    pointerType: e.pointerType,
    startX: e.clientX,
    startY: e.clientY,
    x: e.clientX,
    y: e.clientY,
    started: false,
    k: null,
    scroller: null,
    cleanups: [],
  };
  session = s;

  const move = (ev: PointerEvent) => {
    if (ev.pointerId !== s.pointerId) return;
    s.x = ev.clientX;
    s.y = ev.clientY;
    if (!s.started) {
      const press = classifyPress(s.pointerType, s.x - s.startX, s.y - s.startY);
      if (press === "wait") return;
      if (press === "scroll") return finish(false);
      activate(s, onActivate);
    }
    refresh(s);
  };
  const up = (ev: PointerEvent) => {
    if (ev.pointerId !== s.pointerId) return;
    s.x = ev.clientX;
    s.y = ev.clientY;
    finish(true);
  };
  const abort = () => finish(false);
  const esc = (ev: KeyboardEvent) => {
    // Before the drag starts, Escape still closes the menu the usual way.
    if (ev.key !== "Escape" || !s.started) return;
    ev.preventDefault();
    ev.stopPropagation();
    finish(false);
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
  window.addEventListener("pointercancel", abort);
  window.addEventListener("keydown", esc, true);
  window.addEventListener("blur", abort);
  s.cleanups.unshift(() => {
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
    window.removeEventListener("pointercancel", abort);
    window.removeEventListener("keydown", esc, true);
    window.removeEventListener("blur", abort);
  });
}

/** Releases everything a drag in progress holds (the page is going away). */
export const cancelInsertDrag = () => finish(false);
