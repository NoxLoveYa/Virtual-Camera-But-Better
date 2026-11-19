import { useLayoutEffect, useRef } from "react";
import { useStore } from "../store";
import { selectedFilterId } from "../ui/layout";
import type { ChainHandlers, DragView } from "../ui/useChainDrag";
import { ControlsCard } from "./ControlsCard";
import { FilterInspector } from "./FilterInspector";
import { OutputPanel } from "./OutputPanel";
import { SourcePanel } from "./SourcePanel";

interface Props {
  /** How the selected filter is being dragged or lifted (`null` most of the time) and the reorder handlers for its grip. */
  drag: DragView | null;
  reorder: ChainHandlers;
}

/**
 * The right region: the settings of the one selected item, a section or a filter. It is the host only: each item renders its
 * own header (`InspectorHeader`) and its body (`.insp-scroll`, the one part that scrolls, and only when the item is taller
 * than the column).
 */
export function Inspector({ drag, reorder }: Props) {
  const selected = useStore((s) => s.ui.selected);
  // The store keeps the selection on an item that exists; this is only a net for the frame between two updates.
  const exists = useStore((s) => {
    const id = selectedFilterId(s.ui.selected);
    return id === null || s.chain.some((f) => f.id === id);
  });
  const shows = exists ? selected : "camera";
  const id = selectedFilterId(shows);
  const root = useRef<HTMLElement>(null);

  // Another item starts at its top.
  useLayoutEffect(() => {
    const scroll = root.current?.querySelector<HTMLElement>(".insp-scroll");
    if (scroll) scroll.scrollTop = 0;
  }, [shows]);

  return (
    <aside className="inspector" data-region="inspector" data-shows={shows} aria-label="Inspector" ref={root}>
      {id !== null ? (
        <FilterInspector key={id} id={id} dragMode={drag?.id === id ? drag.mode : null} reorder={reorder} />
      ) : shows === "controls" ? (
        <ControlsCard />
      ) : shows === "output" ? (
        <OutputPanel />
      ) : (
        <SourcePanel />
      )}
    </aside>
  );
}
