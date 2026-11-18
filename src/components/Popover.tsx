import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { createPortal } from "react-dom";

interface Props {
  open: boolean;
  onClose: () => void;
  /** The element that opens the menu; stays visible while it's open. */
  anchor: ReactNode;
  children: ReactNode;
  align?: "left" | "right";
  /** Make the panel at least as wide as the anchor (for dropdowns). */
  matchWidth?: boolean;
  role?: "menu" | "listbox" | "dialog";
  /** The accessible name of the panel. */
  label?: string;
  /** Move the focus into the panel when it opens (its first switch, else its first button), and back to the anchor on Escape. */
  autoFocusFirst?: boolean;
  /** Open upwards when the room above the anchor is larger than the room below (an anchor low in the window). */
  flip?: boolean;
  className?: string;
}

/**
 * A dropdown panel under its anchor that closes on outside click, Escape, resize or page scroll.
 * It is positioned against the window and capped to the room below the anchor, and rendered at the top of the page,
 * so a scrolling or dimmed parent can neither clip it nor affect its backdrop blur.
 */
export function Popover({ open, onClose, anchor, children, align = "left", matchWidth, role = "menu", label, autoFocusFirst, flip, className = "" }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const close = useRef(onClose);
  close.current = onClose;
  const returnFocus = useRef(autoFocusFirst);
  returnFocus.current = autoFocusFirst;
  const [place, setPlace] = useState<CSSProperties>({});

  useLayoutEffect(() => {
    if (!open || !ref.current) return;
    const r = ref.current.getBoundingClientRect();
    const below = window.innerHeight - r.bottom - 14;
    const above = r.top - 14;
    const up = flip === true && below < 300 && above > below;
    setPlace({
      ...(up ? { bottom: window.innerHeight - r.top + 6 } : { top: r.bottom + 6 }),
      ...(align === "left" ? { left: r.left } : { right: window.innerWidth - r.right }),
      maxHeight: Math.max(120, up ? above : below),
      ...(matchWidth && { minWidth: r.width }),
    });
  }, [open, align, matchWidth, flip]);

  useEffect(() => {
    if (!open || !autoFocusFirst) return;
    menu.current?.querySelector<HTMLElement>("[role=switch], button, input, [tabindex]")?.focus({ preventScroll: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const inside = (t: EventTarget | null) => ref.current?.contains(t as Node) || menu.current?.contains(t as Node);
    const away = (e: MouseEvent) => !inside(e.target) && close.current();
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      close.current();
      if (returnFocus.current) ref.current?.querySelector<HTMLElement>("button, input, [tabindex]")?.focus();
    };
    const scrolled = (e: Event) => !inside(e.target) && close.current();
    const resized = () => close.current();
    document.addEventListener("mousedown", away);
    document.addEventListener("keydown", esc);
    window.addEventListener("scroll", scrolled, true);
    window.addEventListener("resize", resized);
    return () => {
      document.removeEventListener("mousedown", away);
      document.removeEventListener("keydown", esc);
      window.removeEventListener("scroll", scrolled, true);
      window.removeEventListener("resize", resized);
    };
  }, [open]);

  return (
    <div className="pop" ref={ref}>
      {anchor}
      {open &&
        createPortal(
          <div className={`pop-menu ${className}`} style={place} role={role} aria-label={label} ref={menu}>
            {children}
          </div>,
          document.body,
        )}
    </div>
  );
}
