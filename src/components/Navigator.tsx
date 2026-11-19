import { memo, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { useShallow } from "zustand/react/shallow";
import { useStore } from "../store";
import { useSummary } from "../ui/useSummary";
import { defOf, extOf, isExtKind, labelOf, type ParamDef } from "../filters/schema";
import { finalIndex } from "../ui/chainDrag";
import { registerField } from "../ui/fileDrop";
import { filterSelection } from "../ui/layout";
import { focusNavRow, focusSelectedRow } from "../ui/navFocus";
import { useInsertView } from "../ui/insertDragSession";
import { moveWithFocus, type ChainHandlers, type useChainDrag } from "../ui/useChainDrag";
import { AddFilter, AddHere, AddPlus } from "./AddFilter";
import { EmptyChain } from "./EmptyChain";
import { Icon, type IconName } from "./Icon";
import { Popover } from "./Popover";

/**
 * The navigator: the three section rows (Camera, Camera controls, Output), then the filter chain as compact rows and the
 * Add filter row. A click selects the row's item, which the inspector then shows. The filter block carries
 * `data-drop="chain"` and holds the list (`ol.chain > li.nav-step[data-filter-id]`), which `ui/chainGeometry.ts` measures
 * for reorder, insert-drag and file drops.
 */

const SECTIONS: { id: "camera" | "controls" | "output"; icon: IconName; title: string }[] = [
  { id: "camera", icon: "camera", title: "Camera" },
  { id: "controls", icon: "sliders", title: "Camera controls" },
  { id: "output", icon: "output", title: "Output" },
];

function SectionRow({ id, icon, title }: (typeof SECTIONS)[number]) {
  const selected = useStore((s) => s.ui.selected === id);
  const summary = useSummary(id);
  return (
    <li>
      <button
        type="button"
        className="nav-row nav-section"
        data-nav={id}
        aria-current={selected ? "true" : undefined}
        title={`${title}: ${summary}`}
        onClick={() => useStore.getState().select(id)}
      >
        <Icon name={icon} size={16} />
        <span className="nav-text">
          <span className="nav-name">{title}</span>
          <span className="nav-summary">{summary}</span>
        </span>
      </button>
    </li>
  );
}

/** The ⋯ of a filter row: Move up, Move down, Remove. */
function RowMenu({ id, index, count, label }: { id: string; index: number; count: number; label: string }) {
  const [open, setOpen] = useState(false);
  const run = (fn: () => void) => () => {
    setOpen(false);
    fn();
  };
  const move = (to: number) => () => {
    moveWithFocus(index, to);
    focusNavRow(id);
  };
  const remove = () => {
    const wasSelected = useStore.getState().ui.selected === filterSelection(id);
    useStore.getState().removeFilter(id);
    if (wasSelected) focusSelectedRow();
  };
  return (
    <Popover
      open={open}
      onClose={() => setOpen(false)}
      align="right"
      className="row-menu"
      flip
      anchor={
        <button
          type="button"
          className="nav-more ghost icon"
          tabIndex={-1}
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={`More for ${label}`}
          title="More"
          onClick={() => setOpen(!open)}
        >
          <Icon name="more" />
        </button>
      }
    >
      <button type="button" role="menuitem" className="pm-item" disabled={index === 0} onClick={run(move(index - 1))}>
        <Icon name="up" /> Move up
      </button>
      <button type="button" role="menuitem" className="pm-item" disabled={index === count - 1} onClick={run(move(index + 1))}>
        <Icon name="down" /> Move down
      </button>
      <button type="button" role="menuitem" className="pm-item" onClick={run(remove)}>
        <Icon name="close" /> Remove
      </button>
    </Popover>
  );
}

const act = () => useStore.getState();

interface RowProps {
  id: string;
  dragMode: "pointer" | "keyboard" | null;
  reorder: ChainHandlers;
}

/** A filter with one file parameter takes a dropped file of a fitting kind on its row (the others take it in the inspector). */
const pathParamOf = (params: readonly ParamDef[] | undefined) => {
  const paths = (params ?? []).filter((p): p is Extract<ParamDef, { type: "path" }> => p.type === "path");
  return paths.length === 1 ? paths[0] : null;
};

/** One filter, one 32 px line: grip, step number, name, bypass switch, ⋯. Reads only its own filter, so a slider moving in the inspector does not render the list. */
const FilterRow = memo(function FilterRow({ id, dragMode: mode, reorder }: RowProps) {
  const filter = useStore((s) => s.chain.find((f) => f.id === id));
  const index = useStore((s) => s.chain.findIndex((f) => f.id === id));
  const count = useStore((s) => s.chain.length);
  const extensions = useStore((s) => s.extensions);
  const extensionsRead = useStore((s) => s.extensionsRead);
  const selected = useStore((s) => s.ui.selected === filterSelection(id));
  const tool = useStore((s) => s.previewTool?.filter === id);
  const [over, setOver] = useState(false);
  const rowRef = useRef<HTMLLIElement>(null);
  const kind = filter?.kind;
  const pathParam = kind ? pathParamOf(defOf(kind, extensions)?.params) : null;
  const pathKey = pathParam?.key;
  const pathKind = pathParam?.filter;

  useEffect(() => {
    const el = rowRef.current;
    if (!el || !pathKey || !pathKind) return;
    return registerField(el, { kind: pathKind, drop: (path) => act().setParam(id, pathKey, path), setOver });
  }, [id, pathKey, pathKind]);

  // A filter that was just removed is still mounted until the list renders again: it has nothing to show then.
  if (!filter) return null;
  const label = labelOf(filter.kind, extensions);
  const ext = extOf(filter.kind, extensions);
  // An extension that is missing, failed to load or is switched off does not run: its row reads as bypassed.
  const idle = isExtKind(filter.kind) && extensionsRead && (!ext || ext.state === "error" || ext.state === "disabled");
  const bypassed = !filter.enabled || idle;
  const movable = count > 1;
  const gripDown = (e: ReactPointerEvent<HTMLElement>) => reorder.onPointerDown(e, id);

  return (
    <li
      ref={rowRef}
      className={`nav-step${selected ? " selected" : ""}${bypassed ? " off" : ""}${movable ? " movable" : ""}${mode === "pointer" ? " dragging" : ""}${mode === "keyboard" ? " lifted" : ""}${tool ? " tooling" : ""}${over ? " drop-over" : ""}`}
      data-filter-id={id}
      data-drop={pathKey ? "path" : undefined}
      data-kind={pathKind}
      onKeyDown={(e) => reorder.onCardKeyDown(e, id)}
    >
      {/* The "+" of the gap above this row. It sits over the gap without taking room, and shows on hover and focus (always on touch). */}
      {index > 0 && (
        <div className="gap-add">
          <AddHere at={index} />
        </div>
      )}
      <button
        type="button"
        className="grip"
        data-act="grip"
        tabIndex={-1}
        title="Drag to reorder"
        aria-label={`Move ${label}, step ${index + 1} of ${count}`}
        aria-disabled={!movable}
        onPointerDown={gripDown}
      >
        <Icon name="grip" />
      </button>
      <button
        type="button"
        className="nav-main"
        aria-current={selected ? "true" : undefined}
        title={over ? "Drop to use this file" : label}
        onPointerDown={gripDown}
        onClick={() => {
          // A drag that started on the row ends with a click on it: that is not a selection.
          if (!reorder.consumeDragClick()) act().select(filterSelection(id));
        }}
      >
        <span className="nav-num" title={`Step ${index + 1} of ${count}`}>
          {String(index + 1).padStart(2, "0")}
        </span>
        <span className="nav-label">{label}</span>
      </button>
      <button
        type="button"
        role="switch"
        className="switch nav-switch"
        tabIndex={-1}
        aria-checked={filter.enabled}
        aria-label={`${label} enabled`}
        title={filter.enabled ? "Bypass" : "Enable"}
        onClick={() => act().toggleFilter(id)}
      />
      <RowMenu id={id} index={index} count={count} label={label} />
    </li>
  );
});

interface Props {
  reorder: ReturnType<typeof useChainDrag>;
  /** Where the drop line of a file dragged in from Explorer goes in the filter block; `null` when none is over it. */
  fileLineTop: number | null;
}

export function Navigator({ reorder, fileLineTop }: Props) {
  // The ids and nothing else: a slider moving in the inspector changes the chain but not this list.
  const ids = useStore(useShallow((s) => s.chain.map((f) => f.id)));
  const insert = useInsertView((s) => s.view);
  const drag = reorder.drag;

  return (
    <nav className="navigator" data-region="nav" aria-label="Sections and filters">
      <div className="nav-scroll">
        <ul className="nav-sections">
          {SECTIONS.map((s) => (
            <SectionRow key={s.id} {...s} />
          ))}
        </ul>
        <div className="nav-rule" role="presentation" />
        <div className="chain-block" ref={reorder.blockRef} data-drop="chain">
          <div className="nav-heading">
            <span className="label" title="Applied top to bottom, camera first">
              Filters
            </span>
            <AddPlus />
          </div>
          <ol className="chain" ref={reorder.chainRef}>
            {ids.length === 0 && <EmptyChain />}
            {ids.map((id) => (
              <FilterRow key={id} id={id} dragMode={drag?.id === id ? drag.mode : null} reorder={reorder.handlers} />
            ))}
          </ol>
          <div className="nav-addrow">
            <AddFilter />
          </div>
          {drag && (
            <div className={`drop-line${finalIndex(drag.from, drag.k) === drag.from ? " noop" : ""}`} style={{ top: reorder.lineTop }} aria-hidden />
          )}
          {fileLineTop !== null && <div className="drop-line file-line" style={{ top: fileLineTop }} aria-hidden />}
          {insert?.lineTop != null && <div className="drop-line insert-line" style={{ top: insert.lineTop }} aria-hidden />}
        </div>
      </div>
    </nav>
  );
}
