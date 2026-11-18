/**
 * Files dropped from Explorer (W2). Tauri's webview handler (`dragDropEnabled`, on by default) takes the OS drop and
 * hands the paths and a position in physical pixels to `onDragDropEvent`; this hook turns that into CSS pixels, finds
 * what is under the pointer (a path field, the chain, or the rest of the window) and acts on it. What a drop means is
 * decided by the pure `routeDrop` in `fileDrop.ts`.
 *
 * In debug builds `window.__betterCam` runs the same code as the listener (after the coordinate conversion), because a
 * test driven through the DevTools protocol cannot make an OS drop.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useStore } from "../store";
import { insertionIndex } from "./chainDrag";
import { fieldAt, firstSupported, hintFor, routeDrop, type DropTarget, type FieldTarget } from "./fileDrop";

/** `npm run tauri dev` (the Vite dev server); false in a release build. The project has no `vite/client` types. */
const IS_DEV_BUILD = (import.meta as unknown as { env?: { DEV?: boolean } }).env?.DEV === true;

/** The "Use as" question for a `.png`. `x` and `y` are CSS pixels, where the file was dropped. */
export interface Chooser {
  x: number;
  y: number;
  path: string;
  at: number | null;
  note: string | null;
}

export interface FileDropView {
  /** The pill above the footer; `null` when no usable file is being dragged. */
  hint: string | null;
  /** Where the drop line goes in `.chain-block` while the pointer is over the chain, else `null`. */
  lineTop: number | null;
  chooser: Chooser | null;
}

const IDLE: FileDropView = { hint: null, lineTop: null, chooser: null };

/** The gap of the chain under client `y`, and where its line goes (relative to `.chain-block`). `null` without a chain block. */
function chainGap(block: Element, y: number): { gap: number; lineTop: number } | null {
  const ol = block.querySelector(".chain");
  if (!ol) return null;
  const items = Array.from(ol.children) as HTMLElement[];
  if (items.length === 0) return null;
  // An empty chain shows its hint between the terminals: the line goes at the top of that box, not through the text.
  const empty = ol.querySelector(".empty");
  if (empty && !items.some((li) => li.classList.contains("step"))) {
    return { gap: 0, lineTop: Math.round(empty.getBoundingClientRect().top - block.getBoundingClientRect().top - 1) };
  }
  const cards = items.filter((li) => li.classList.contains("step")).map((li) => (li.querySelector(".card") ?? li).getBoundingClientRect());
  const first = items[0].getBoundingClientRect();
  const last = items[items.length - 1].getBoundingClientRect();
  const gap = insertionIndex(cards.map((r) => r.top + r.height / 2), y);
  const above = gap === 0 ? first.bottom : cards[gap - 1].bottom;
  const below = gap === cards.length ? last.top : cards[gap].top;
  return { gap, lineTop: Math.round((above + below) / 2 - block.getBoundingClientRect().top - 1) };
}

interface Under {
  target: DropTarget;
  field: FieldTarget | null;
  lineTop: number | null;
}

/** What is at the CSS point (x, y): a path field, the chain (with its gap), or the rest of the window. */
function under(x: number, y: number): Under {
  const el = document.elementFromPoint(x, y);
  const field = fieldAt(el);
  const block = el?.closest('[data-drop="chain"]');
  const hit = block ? chainGap(block, y) : null;
  return { target: { field: field?.kind, gap: hit?.gap ?? null }, field, lineTop: hit?.lineTop ?? null };
}

export function useFileDrop() {
  const [view, setView] = useState<FileDropView>(IDLE);
  const hovered = useRef<FieldTarget | null>(null);
  /** The files of the drag in progress, set by `enter`. */
  const dragging = useRef<string[] | null>(null);
  /** The open "Use as" question (also in `view`; the ref is what `choose` reads, outside of any render). */
  const chooser = useRef<Chooser | null>(null);

  const highlight = (field: FieldTarget | null) => {
    if (hovered.current === field) return;
    hovered.current?.setOver(false);
    field?.setOver(true);
    hovered.current = field;
  };

  const clear = useCallback(() => {
    dragging.current = null;
    highlight(null);
    document.body.classList.remove("file-drag");
    setView((v) => (v.hint === null && v.lineTop === null ? v : { ...v, hint: null, lineTop: null }));
  }, []);

  /** The NVIDIA dialog is a modal: it takes no drops and shows no highlight. */
  const blocked = () => useStore.getState().nvidiaPrompt;

  const update = useCallback(
    (x: number, y: number) => {
      const paths = dragging.current;
      const file = paths && !blocked() ? firstSupported(paths) : null;
      if (!file) {
        highlight(null);
        document.body.classList.remove("file-drag");
        setView((v) => (v.hint === null && v.lineTop === null ? v : { ...v, hint: null, lineTop: null }));
        return;
      }
      const at = under(x, y);
      const route = routeDrop([file], at.target);
      const onField = route.action.type === "set-path";
      highlight(onField ? at.field : null);
      document.body.classList.add("file-drag");
      const hint = hintFor(file, onField);
      const lineTop = onField ? null : at.lineTop;
      setView((v) => (v.hint === hint && v.lineTop === lineTop ? v : { ...v, hint, lineTop }));
    },
    [],
  );

  const enter = useCallback(
    (paths: string[], x: number, y: number) => {
      dragging.current = paths;
      update(x, y);
    },
    [update],
  );

  /** The answer to the "Use as" question: a filter to add, or `null` to cancel. */
  const choose = useCallback((use: "clut_filter" | "mask_filter" | null) => {
    const c = chooser.current;
    if (!c) return;
    chooser.current = null;
    setView((v) => ({ ...v, chooser: null }));
    if (!use) return;
    const s = useStore.getState();
    s.addFilter(use, c.at, { image_path: c.path });
    if (c.note) s.addNotice(c.note);
  }, []);

  const drop = useCallback(
    async (paths: string[], x: number, y: number) => {
      const wasBlocked = blocked();
      clear();
      if (wasBlocked) return;
      const at = under(x, y);
      const { action, note } = routeDrop(paths, at.target);
      const s = useStore.getState();
      switch (action.type) {
        case "set-path":
          at.field?.drop(action.path);
          if (note) s.notify(note);
          return;
        case "add":
          s.addFilter(action.filter, action.at, { image_path: action.path });
          if (note) s.addNotice(note);
          return;
        case "choose":
          chooser.current = { x, y, path: action.path, at: action.at, note };
          setView((v) => ({ ...v, chooser: chooser.current }));
          return;
        case "preset":
          await s.importPreset(action.path);
          if (note) useStore.getState().addNotice(note);
          return;
        case "unsupported":
          s.notify(action.message);
          return;
      }
    },
    [clear],
  );

  useEffect(() => {
    // One listener for the app's lifetime. A drop that arrives while another is being acted on is still handled.
    let unlisten: (() => void) | null = null;
    let stopped = false;
    const toCss = (p: { x: number; y: number }) => ({ x: p.x / window.devicePixelRatio, y: p.y / window.devicePixelRatio });
    try {
      void getCurrentWebview()
        .onDragDropEvent((e) => {
          const p = e.payload;
          if (p.type === "leave") return clear();
          const { x, y } = toCss(p.position);
          if (p.type === "enter") enter(p.paths, x, y);
          else if (p.type === "over") update(x, y);
          else void drop(p.paths, x, y);
        })
        .then((fn) => {
          if (stopped) fn();
          else {
            unlisten = fn;
            if (IS_DEV_BUILD) (window as unknown as { __betterCam?: { listening?: boolean } }).__betterCam!.listening = true;
          }
        })
        .catch((err) => console.error("Better Cam: file drops are not available:", err));
    } catch (err) {
      // Not inside a Tauri webview (the page opened in a plain browser).
      console.warn("Better Cam: file drops are not available:", err);
    }
    if (IS_DEV_BUILD) {
      // `toast` shows a message as the app would; `set` patches the store (a test puts the app in a state it cannot reach without
      // elevation or a camera, such as an outdated virtual camera).
      (window as unknown as { __betterCam: unknown }).__betterCam = {
        listening: false,
        drop,
        enter,
        over: update,
        leave: clear,
        toast: (text: string) => useStore.getState().notify(text),
        set: (patch: Record<string, unknown>) => useStore.setState(patch),
      };
    }
    return () => {
      stopped = true;
      unlisten?.();
      clear();
    };
  }, [clear, drop, enter, update]);

  return { ...view, choose };
}
