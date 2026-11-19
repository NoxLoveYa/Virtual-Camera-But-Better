import { memo, useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from "react";
import { useShallow } from "zustand/react/shallow";
import { useStore } from "../store";
import { useSummary } from "../ui/useSummary";
import { findCamera, findStored, slowCameraText, statusOf } from "../camera/schema";
import { defOf, extIdOf, extOf, isExtKind, labelOf, type ParamDef } from "../filters/schema";
import { finalIndex } from "../ui/chainDrag";
import { registerField } from "../ui/fileDrop";
import { filterSelection, selectedFilterId } from "../ui/layout";
import { focusInspector, focusNavRow, focusSelectedRow } from "../ui/navFocus";
import { cameraDot, controlsDot, extNoteOf, needsOf, rowStatus, type Dot } from "../ui/navSummary";
import { useInsertView } from "../ui/insertDragSession";
import { moveWithFocus, type ChainHandlers, type useChainDrag } from "../ui/useChainDrag";
import { AddFilter, AddHere, AddPlus } from "./AddFilter";
import { useSlowCamera } from "./AutoControls";
import { EmptyChain } from "./EmptyChain";
import { Icon, type IconName } from "./Icon";
import { NavBar, useFlyout } from "./NavRail";
import { Popover } from "./Popover";

/**
 * The navigator: the three section rows (Camera, Camera controls, Output), then the filter chain as compact rows and the
 * Add filter row. A click selects the row's item, which the inspector then shows. The filter block carries
 * `data-drop="chain"` and holds the list (`ol.chain > li.nav-step[data-filter-id]`), which `ui/chainGeometry.ts` measures
 * for reorder, insert-drag and file drops.
 *
 * Below 1100 px (or collapsed by the user) it is the icon rail: the same rows as 52 px icons and chips, and the full
 * navigator as a flyout (`NavRail.tsx`). It works from the keyboard: one row is in the tab order (the selected one), the
 * arrows move and select, Space bypasses, Delete removes, Enter and F6 go to the inspector.
 */

const SECTIONS: { id: "camera" | "controls" | "output"; icon: IconName; title: string }[] = [
  { id: "camera", icon: "camera", title: "Camera" },
  { id: "controls", icon: "sliders", title: "Camera controls" },
  { id: "output", icon: "output", title: "Output" },
];

/** The 8 px status dot of a row, with its sentence as the tooltip. */
function DotView({ dot }: { dot: Dot | null }) {
  if (!dot) return null;
  return <span className="nav-dot" data-tone={dot.tone} role="img" aria-label={dot.text} title={dot.text} />;
}

function CameraDot() {
  const source = useStore((s) => s.source);
  const cameras = useStore((s) => s.cameras);
  const inUse = useStore((s) => s.inUse);
  const cam = cameras ? findCamera(cameras, source.video_device_id) : null;
  return <DotView dot={cameraDot(source, cameras, cam, inUse)} />;
}

function ControlsDot() {
  const cameras = useStore((s) => s.cameras);
  const deviceId = useStore((s) => s.source.video_device_id);
  const stored = useStore((s) => s.controls);
  const autoStatus = useStore((s) => s.autoStatus);
  const slow = useSlowCamera();
  const cam = cameras ? findCamera(cameras, deviceId) : null;
  const device = cam ? (findStored(stored, cam)?.device ?? null) : null;
  const loops = { exposure: statusOf(autoStatus, cam, "exposure"), white_balance: statusOf(autoStatus, cam, "white_balance") };
  return <DotView dot={controlsDot(device, loops, slow ? slowCameraText(slow.measured, slow.requested) : null)} />;
}

interface SectionProps {
  id: "camera" | "controls" | "output";
  icon: IconName;
  title: string;
  /** The icon-only look of the rail. */
  compact: boolean;
  onPick: () => void;
}

function SectionRow({ id, icon, title, compact, onPick }: SectionProps) {
  const selected = useStore((s) => s.ui.selected === id);
  const summary = useSummary(id);
  return (
    <li>
      <button
        type="button"
        className="nav-row nav-section"
        data-nav={id}
        tabIndex={selected ? 0 : -1}
        aria-current={selected ? "true" : undefined}
        aria-label={compact ? `${title}, ${summary}` : undefined}
        title={`${title}: ${summary}`}
        onClick={() => {
          useStore.getState().select(id);
          onPick();
        }}
      >
        <Icon name={icon} size={16} />
        {!compact && (
          <span className="nav-text">
            <span className="nav-name">{title}</span>
            <span className="nav-summary">{summary}</span>
          </span>
        )}
        {id === "camera" && <CameraDot />}
        {id === "controls" && <ControlsDot />}
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
  compact: boolean;
  onPick: () => void;
}

/** A filter with one file parameter takes a dropped file of a fitting kind on its row (the others take it in the inspector). */
const pathParamOf = (params: readonly ParamDef[] | undefined) => {
  const paths = (params ?? []).filter((p): p is Extract<ParamDef, { type: "path" }> => p.type === "path");
  return paths.length === 1 ? paths[0] : null;
};

/**
 * One filter, one 32 px line: grip, step number, name, status dot, bypass switch, ⋯. In the rail it is a 28 px chip: the step
 * number and the dot. Reads only its own filter, so a slider moving in the inspector does not render the list.
 */
const FilterRow = memo(function FilterRow({ id, dragMode: mode, reorder, compact, onPick }: RowProps) {
  const filter = useStore((s) => s.chain.find((f) => f.id === id));
  const index = useStore((s) => s.chain.findIndex((f) => f.id === id));
  const count = useStore((s) => s.chain.length);
  const extensions = useStore((s) => s.extensions);
  const extensionsRead = useStore((s) => s.extensionsRead);
  const selected = useStore((s) => s.ui.selected === filterSelection(id));
  const tool = useStore((s) => s.previewTool?.filter === id);
  const error = useStore((s) => s.filterErrors[id]);
  const loading = useStore((s) => s.filterStatus[id]);
  const gpu = useStore((s) => s.gpu);
  const gpuRead = useStore((s) => s.gpuRead);
  const nvidia = useStore((s) => s.nvidia);
  const [over, setOver] = useState(false);
  const rowRef = useRef<HTMLLIElement>(null);
  const kind = filter?.kind;
  const def = kind ? defOf(kind, extensions) : undefined;
  const pathParam = pathParamOf(def?.params);
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
  // An extension that is missing, failed to load or is switched off does not run: its row reads as bypassed.
  const extNote = isExtKind(filter.kind) && extensionsRead ? extNoteOf(extIdOf(filter.kind), extOf(filter.kind, extensions)) : null;
  const bypassed = !filter.enabled || extNote !== null;
  const status = rowStatus({ needs: needsOf(def?.rtx, gpuRead, gpu, nvidia)?.text ?? null, ext: extNote, error, loading });
  const movable = count > 1;
  const gripDown = (e: ReactPointerEvent<HTMLElement>) => reorder.onPointerDown(e, id);
  const step = String(index + 1).padStart(2, "0");

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
      {!compact && index > 0 && (
        <div className="gap-add">
          <AddHere at={index} />
        </div>
      )}
      {!compact && (
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
      )}
      <button
        type="button"
        className="nav-main"
        tabIndex={selected ? 0 : -1}
        aria-current={selected ? "true" : undefined}
        aria-label={compact ? `${label}, step ${index + 1} of ${count}${bypassed ? ", bypassed" : ""}` : undefined}
        title={over ? "Drop to use this file" : label}
        onPointerDown={gripDown}
        onClick={() => {
          // A drag that started on the row ends with a click on it: that is not a selection.
          if (reorder.consumeDragClick()) return;
          act().select(filterSelection(id));
          onPick();
        }}
      >
        <span className="nav-num" title={`Step ${index + 1} of ${count}`}>
          {step}
        </span>
        {!compact && <span className="nav-label">{label}</span>}
      </button>
      {status?.kind === "dot" && <span className="nav-dot" data-tone={status.tone} role="img" aria-label={status.text} title={status.text} />}
      {status?.kind === "loading" && <span className="spinner nav-spin" role="status" aria-label={status.text} title={status.text} />}
      {!compact && (
        <>
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
        </>
      )}
    </li>
  );
});

interface Props {
  reorder: ReturnType<typeof useChainDrag>;
  /** Where the drop line of a file dragged in from Explorer goes in the filter block; `null` when none is over it. */
  fileLineTop: number | null;
  /** The navigator is the icon rail (below 1100 px, or collapsed by the user). */
  rail: boolean;
  /** The window is wide enough for the docked navigator, so the flyout can dock it again. */
  canDock: boolean;
}

/** The row buttons of the navigator, top to bottom: the three sections, then the filters. */
const ROWS = "[data-nav], li.nav-step > .nav-main";

/** What a row button selects. */
const selectionOf = (row: HTMLElement): string | null => {
  const filterId = row.closest<HTMLElement>("li.nav-step")?.dataset.filterId;
  return filterId ? filterSelection(filterId) : (row.dataset.nav ?? null);
};

export function Navigator({ reorder, fileLineTop, rail, canDock }: Props) {
  // The ids and nothing else: a slider moving in the inspector changes the chain but not this list.
  const ids = useStore(useShallow((s) => s.chain.map((f) => f.id)));
  const insert = useInsertView((s) => s.view);
  const drag = reorder.drag;
  const panelRef = useRef<HTMLDivElement>(null);
  const flyout = useFlyout(rail, panelRef);
  const compact = rail && !flyout.open;
  const { pick } = flyout;

  /** Arrows, Home and End move the selection with the focus; Space bypasses, Delete removes, Enter and F6 go to the inspector. */
  const onKeyDown = (e: ReactKeyboardEvent<HTMLElement>) => {
    const target = e.target as HTMLElement;
    // A menu opened from a row is in a portal, but its keys still bubble through here.
    if (e.defaultPrevented || target.closest(".pop-menu")) return;
    if (e.key === "F6" && !e.shiftKey && !e.altKey && !e.ctrlKey) {
      e.preventDefault();
      flyout.close(false);
      focusInspector();
      return;
    }
    const row = target.closest<HTMLElement>(ROWS);
    if (!row || e.altKey || e.ctrlKey || e.metaKey) return;
    const filterId = row.closest<HTMLElement>("li.nav-step")?.dataset.filterId ?? null;

    switch (e.key) {
      case "ArrowUp":
      case "ArrowDown":
      case "Home":
      case "End": {
        const rows = Array.from(e.currentTarget.querySelectorAll<HTMLElement>(ROWS));
        const at = rows.indexOf(row);
        const to = e.key === "Home" ? 0 : e.key === "End" ? rows.length - 1 : Math.max(0, Math.min(rows.length - 1, at + (e.key === "ArrowDown" ? 1 : -1)));
        e.preventDefault();
        const next = rows[to];
        const selection = next && selectionOf(next);
        if (!next || selection === null || next === row) return;
        act().select(selection);
        next.focus({ preventScroll: true });
        next.scrollIntoView({ block: "nearest" });
        const nextId = selectedFilterId(selection);
        const { chain, extensions } = act();
        const i = chain.findIndex((f) => f.id === nextId);
        reorder.handlers.announce(
          i >= 0 ? `${labelOf(chain[i].kind, extensions)} selected, step ${i + 1} of ${chain.length}.` : `${SECTIONS.find((s) => s.id === selection)?.title ?? selection} selected.`,
        );
        break;
      }
      case "Enter":
        // The row's own click would only select what is selected already.
        e.preventDefault();
        flyout.close(false);
        focusInspector();
        break;
      case " ":
        if (filterId) {
          e.preventDefault();
          act().toggleFilter(filterId);
        }
        break;
      case "Delete":
        if (filterId) {
          e.preventDefault();
          const wasSelected = act().ui.selected === filterSelection(filterId);
          act().removeFilter(filterId);
          if (wasSelected) focusSelectedRow();
        }
        break;
    }
  };

  /** Space on a button clicks it when the key comes up; on a filter row the key already bypassed it. */
  const onKeyUp = (e: ReactKeyboardEvent<HTMLElement>) => {
    if (e.key === " " && (e.target as HTMLElement).matches("li.nav-step > .nav-main")) e.preventDefault();
  };

  return (
    <nav className="navigator" data-region="nav" aria-label="Sections and filters" onKeyDown={onKeyDown} onKeyUp={onKeyUp}>
      <div ref={panelRef} className={`nav-panel${flyout.open ? " nav-flyout" : ""}${compact ? " compact" : ""}`}>
        <NavBar rail={rail} flyout={flyout} canDock={canDock} />
        <div className="nav-scroll">
          <ul className="nav-sections">
            {SECTIONS.map((s) => (
              <SectionRow key={s.id} {...s} compact={compact} onPick={pick} />
            ))}
          </ul>
          <div className="nav-rule" role="presentation" />
          <div className="chain-block" ref={reorder.blockRef} data-drop="chain">
            {!compact && (
              <div className="nav-heading">
                <span className="label" title="Applied top to bottom, camera first">
                  Filters
                </span>
                <AddPlus />
              </div>
            )}
            <ol className="chain" ref={reorder.chainRef}>
              {ids.length === 0 && <EmptyChain compact={compact} />}
              {ids.map((id) => (
                <FilterRow key={id} id={id} dragMode={drag?.id === id ? drag.mode : null} reorder={reorder.handlers} compact={compact} onPick={pick} />
              ))}
            </ol>
            <div className="nav-addrow">
              <AddFilter compact={compact} />
            </div>
            {drag && (
              <div className={`drop-line${finalIndex(drag.from, drag.k) === drag.from ? " noop" : ""}`} style={{ top: reorder.lineTop }} aria-hidden />
            )}
            {fileLineTop !== null && <div className="drop-line file-line" style={{ top: fileLineTop }} aria-hidden />}
            {insert?.lineTop != null && <div className="drop-line insert-line" style={{ top: insert.lineTop }} aria-hidden />}
          </div>
        </div>
      </div>
    </nav>
  );
}
