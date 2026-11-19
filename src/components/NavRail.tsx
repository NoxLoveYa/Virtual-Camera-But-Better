import { useCallback, useEffect, useRef, useState, type RefObject } from "react";
import { useStore } from "../store";
import { focusSelectedRow } from "../ui/navFocus";
import { Icon } from "./Icon";

/**
 * The navigator below 1100 px (or collapsed by the user) is a 52 px icon rail. The full navigator comes back as a flyout: the
 * same panel, 232 px wide, over the left of the preview, which closes on Escape, on a click outside and after a pick.
 * (docs/LAYOUT-INSPECTOR.md 1.6.) The rail, the flyout and the docked navigator are one DOM, so the reorder, insert-drag and
 * file-drop code that finds the rows by their selectors works in all three.
 */

export interface Flyout {
  open: boolean;
  show: () => void;
  /** Closes it; `returnFocus` puts the focus on the » button (not after a click elsewhere, which already moved it). */
  close: (returnFocus?: boolean) => void;
  /** A row was picked with the pointer: the flyout has done its job. */
  pick: () => void;
}

export function useFlyout(rail: boolean, panel: RefObject<HTMLElement | null>): Flyout {
  const [open, setOpen] = useState(false);
  const shown = rail && open;
  const shownRef = useRef(false);
  shownRef.current = shown;

  const close = useCallback(
    (returnFocus = true) => {
      setOpen(false);
      if (returnFocus) requestAnimationFrame(() => panel.current?.querySelector<HTMLElement>("[data-nav-toggle]")?.focus());
    },
    [panel],
  );
  const show = useCallback(() => setOpen(true), []);
  const pick = useCallback(() => {
    if (shownRef.current) close(true);
  }, [close]);

  // The window grew past 1100 px: the docked navigator replaces the rail, and the flyout has nothing left to be.
  useEffect(() => {
    if (!rail) setOpen(false);
  }, [rail]);

  // The focus goes into the flyout when it opens: onto the selected row.
  useEffect(() => {
    if (shown) focusSelectedRow();
  }, [shown]);

  useEffect(() => {
    if (!shown) return;
    const away = (e: PointerEvent) => {
      const t = e.target as Element | null;
      // A menu of the flyout (⋯, Add filter) lives at the end of the page, but belongs to it.
      if (!t || panel.current?.contains(t) || t.closest(".pop-menu")) return;
      close(false);
    };
    // In the capture phase, before a menu's own Escape handler closes it: an Escape that closes a menu does not close the flyout.
    // (A drag in progress takes its Escape first, in the capture phase of the window.)
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented || document.querySelector(".pop-menu")) return;
      e.preventDefault();
      close(true);
    };
    document.addEventListener("pointerdown", away, true);
    document.addEventListener("keydown", esc, true);
    return () => {
      document.removeEventListener("pointerdown", away, true);
      document.removeEventListener("keydown", esc, true);
    };
  }, [shown, close, panel]);

  return { open: shown, show, close, pick };
}

interface BarProps {
  rail: boolean;
  flyout: Flyout;
  /** The window is wide enough for the docked navigator (the rail is the user's choice, not the window's). */
  canDock: boolean;
}

/**
 * The strip at the top of the navigator, one button whose job follows the mode: » opens the flyout from the rail; in the
 * flyout it docks the navigator again (wide windows) or closes it; in the docked navigator it collapses it to the rail.
 */
export function NavBar({ rail, flyout, canDock }: BarProps) {
  let mode: "open" | "dock" | "close" | "collapse";
  if (!rail) mode = "collapse";
  else if (!flyout.open) mode = "open";
  else mode = canDock ? "dock" : "close";

  const text = { open: "Show navigator", dock: "Dock navigator", close: "Close navigator", collapse: "Collapse navigator to icons" }[mode];
  const icon = { open: "chevronsRight", dock: "dock", close: "chevronsLeft", collapse: "chevronsLeft" }[mode] as "chevronsRight" | "dock" | "chevronsLeft";
  const click = () => {
    const { setUi } = useStore.getState();
    if (mode === "open") flyout.show();
    else if (mode === "close") flyout.close(true);
    else setUi({ navRail: mode === "collapse" });
  };

  return (
    <div className="nav-bar">
      <button
        type="button"
        className="nav-toggle ghost icon"
        data-nav-toggle={mode}
        aria-label={text}
        aria-expanded={mode === "open" ? false : mode === "close" || mode === "dock" ? true : undefined}
        title={text}
        onClick={click}
      >
        <Icon name={icon} size={16} />
      </button>
    </div>
  );
}
