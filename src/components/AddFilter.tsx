import { useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { useStore } from "../store";
import { FILTERS, FILTER_HINTS, type BuiltinKind, type FilterKind } from "../filters/schema";
import { beginInsertDrag } from "../ui/insertDragSession";
import { gapLabel } from "../ui/insertDrag";
import { focusNavRow } from "../ui/navFocus";
import { Icon } from "./Icon";
import { Popover } from "./Popover";

/** The arrow keys walk the menu's items (the menu is in a portal, so Tab alone would leave it). */
function walkMenu(e: ReactKeyboardEvent<HTMLElement>) {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  const items = Array.from(e.currentTarget.closest(".pop-menu")?.querySelectorAll<HTMLElement>(".pm-item") ?? []);
  const i = items.indexOf(e.currentTarget);
  if (i < 0 || items.length === 0) return;
  e.preventDefault();
  items[(i + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length].focus();
}

interface MenuProps {
  /** Where the filter goes: the index it gets (0 = first); omitted, at the end. */
  at?: number;
  open: boolean;
  setOpen: (open: boolean) => void;
  anchor: ReactNode;
  align: "left" | "right";
  /** Opened from a "+" in the chain: the focus goes into the menu. */
  here?: boolean;
}

/**
 * The menu of filters, shared by the navigator's "+" and Add filter row and the "+" between two rows. A filter can also be
 * dragged out of it into the chain. The new filter is selected (the store does that) and its row takes the focus.
 */
function AddMenu({ at, open, setOpen, anchor, align, here }: MenuProps) {
  const addFilter = useStore((s) => s.addFilter);
  // The NVIDIA filters can still be added without an RTX GPU (a preset may carry them); the menu says what they need.
  const noRtx = useStore((s) => s.gpuRead && !s.gpu);
  const extensions = useStore((s) => s.extensions);
  // Only an extension that is ready can be added; overlays and widgets have their own place (a later version).
  const ready = extensions.filter((e) => e.kind === "filter" && e.state === "ready");

  const add = (kind: FilterKind) => {
    setOpen(false);
    focusNavRow(addFilter(kind, at));
  };
  /** A press on an item is a click until the pointer moves: then the menu closes and the filter follows the pointer into the chain. */
  const press = (kind: FilterKind) => (e: ReactPointerEvent<HTMLElement>) => beginInsertDrag(e, kind, () => setOpen(false));

  return (
    <Popover open={open} onClose={() => setOpen(false)} align={align} className="add-menu" anchor={anchor} autoFocusFirst={here} flip>
      {(Object.keys(FILTERS) as BuiltinKind[]).map((kind) => {
        const rtxOnly = noRtx && FILTERS[kind].rtx === true;
        return (
          <button key={kind} className="pm-item two" data-kind={kind} onClick={() => add(kind)} onPointerDown={press(kind)} onKeyDown={walkMenu}>
            <b>
              {FILTERS[kind].label}
              {rtxOnly && <span className="rtx-badge">RTX only</span>}
            </b>
            <small>{rtxOnly ? FILTER_HINTS[kind].replace(/ \(RTX only\)$/, "") : FILTER_HINTS[kind]}</small>
          </button>
        );
      })}
      {ready.length > 0 && (
        <div className="pm-group" role="presentation">
          Extension filters
        </div>
      )}
      {ready.map((ext) => (
        <button
          key={ext.id}
          className="pm-item two"
          data-ext={ext.id}
          data-kind={`ext:${ext.id}`}
          onClick={() => add(`ext:${ext.id}`)}
          onPointerDown={press(`ext:${ext.id}`)}
          onKeyDown={walkMenu}
        >
          <b>
            {ext.name}
            <span className="ext-badge">Extension</span>
          </b>
          {ext.hint && <small>{ext.hint}</small>}
        </button>
      ))}
    </Popover>
  );
}

/** The "Add filter" row at the end of the navigator's list: adds at the end, or drag an item into the chain. */
export function AddFilter({ compact = false }: { compact?: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <AddMenu
      open={open}
      setOpen={setOpen}
      align="left"
      anchor={
        <button
          className="nav-add"
          data-add-filter
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={compact ? "Add filter" : undefined}
          title={compact ? "Add filter" : undefined}
          onClick={() => setOpen(!open)}
        >
          <Icon name="plus" />
          {!compact && " Add filter"}
        </button>
      }
    />
  );
}

/** The 24 px "+" on the FILTERS heading: the same menu, adds at the end. */
export function AddPlus() {
  const [open, setOpen] = useState(false);
  return (
    <AddMenu
      open={open}
      setOpen={setOpen}
      align="left"
      anchor={
        <button className="nav-plus ghost" aria-haspopup="menu" aria-expanded={open} aria-label="Add filter" title="Add filter" onClick={() => setOpen(!open)}>
          <Icon name="plus" />
        </button>
      }
    />
  );
}

/** The "+" in the gap above the row at index `at` (so between step `at` and `at + 1`): opens the same menu and inserts there. */
export function AddHere({ at }: { at: number }) {
  const [open, setOpen] = useState(false);
  return (
    <AddMenu
      at={at}
      open={open}
      setOpen={setOpen}
      align="left"
      here
      anchor={
        <button className="gap-btn" data-gap={at} aria-haspopup="menu" aria-expanded={open} aria-label={gapLabel(at)} title={gapLabel(at)} onClick={() => setOpen(!open)}>
          <Icon name="plus" size={12} />
        </button>
      }
    />
  );
}
