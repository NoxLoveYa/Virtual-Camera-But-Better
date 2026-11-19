import { useLayoutEffect, useRef } from "react";
import { useStore } from "../store";
import { selectedFilterId } from "../ui/layout";
import type { ChainHandlers, DragView } from "../ui/useChainDrag";
import { ControlsCard } from "./ControlsCard";
import { FilterCard } from "./FilterCard";
import { OutputPanel } from "./OutputPanel";
import { SourcePanel } from "./SourcePanel";

interface Props {
  /** How the selected filter is being dragged or lifted (`null` most of the time) and the reorder handlers for its grip. */
  drag: DragView | null;
  reorder: ChainHandlers;
}

/**
 * The right region: the settings of the one selected item, a section or a filter, in a column that scrolls inside itself
 * (`.insp-scroll`) when that item is taller than it. It is the host only: each item still renders its own component.
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
  const scroll = useRef<HTMLDivElement>(null);

  // Another item starts at its top.
  useLayoutEffect(() => {
    if (scroll.current) scroll.current.scrollTop = 0;
  }, [shows]);

  return (
    <aside className="inspector" data-region="inspector" data-shows={shows} aria-label="Inspector">
      <div className="insp-scroll" ref={scroll}>
        {id !== null ? (
          <FilterCard key={id} id={id} dragMode={drag?.id === id ? drag.mode : null} reorder={reorder} />
        ) : shows === "controls" ? (
          <ControlsCard />
        ) : shows === "output" ? (
          <OutputPanel />
        ) : (
          <SourcePanel />
        )}
      </div>
    </aside>
  );
}
